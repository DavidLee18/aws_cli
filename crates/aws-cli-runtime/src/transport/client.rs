//! The shared, pooled HTTP client.
//!
//! One client for the whole process, built once and reused. This is the point of the
//! rewrite: the previous transport constructed a fresh agent per call site, so every
//! request — every part of every multipart upload, every page of a listing — paid a new
//! TCP and TLS handshake. Keeping connections alive turns that per-request cost into a
//! per-process one.
//!
//! The client is async internally because the pool and the S3 transfer engine want many
//! requests in flight at once, but it exposes a blocking API so callers that make a
//! single request stay straightforward. Both share one runtime.

use crate::transport::body::Body;
use crate::RuntimeError;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Frame;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::OnceLock;
use std::time::Duration;

/// Idle connections kept per host. S3 transfers run dozens of requests at once and
/// dropping a connection between parts would put the handshake straight back.
const POOL_MAX_IDLE_PER_HOST: usize = 64;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

type HttpsClient = Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, BoxBody<Bytes, std::io::Error>>;

/// Transport-level options from the global arguments.
#[derive(Debug, Clone)]
pub struct Transport {
    pub verify_ssl: bool,
    pub ca_bundle: Option<String>,
    pub read_timeout: Option<u64>,
    pub connect_timeout: Option<u64>,
}

impl Default for Transport {
    fn default() -> Self {
        Transport { verify_ssl: true, ca_bundle: None, read_timeout: None, connect_timeout: None }
    }
}

impl Transport {
    fn read_timeout(&self) -> Duration {
        Duration::from_secs(self.read_timeout.unwrap_or(60))
    }
}

/// A request ready to send: fully-formed URL and headers, plus a body that may still be
/// on disk.
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

/// The status and headers of a response, without its body.
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A response whose body has been read into memory.
///
/// Used for API calls, where bodies are documents of at most a few megabytes. Object
/// downloads go through [`send_to_writer`] instead and never build one of these.
pub struct Response {
    pub status: u16,
    body: Bytes,
    headers: Vec<(String, String)>,
}

impl Response {
    /// The raw response bytes. Bytes rather than a `String` because responses carry
    /// arbitrary binary content; decoding everything as UTF-8 would corrupt it.
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    pub fn into_bytes(self) -> Bytes {
        self.body
    }

    /// The body as text, for the protocol parsers and error documents.
    ///
    /// Lossy on purpose: a malformed body should surface as a parse error naming the
    /// service, not as a decoding error from the transport.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

/// The process-wide runtime. Multi-threaded so concurrent transfers actually overlap.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to start the HTTP runtime")
    })
}

/// The process-wide client.
///
/// Built from the first [`Transport`] it sees: the connect timeout and the TLS
/// configuration live on the connector, so they are fixed for the process. Read timeouts
/// are applied per request and stay adjustable. A single CLI invocation resolves the
/// global arguments once, so every caller passes the same values.
fn client(transport: &Transport) -> Result<&'static HttpsClient, RuntimeError> {
    static CLIENT: OnceLock<Result<HttpsClient, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_client(transport))
        .as_ref()
        .map_err(|e| RuntimeError::Http(e.clone()))
}

fn build_client(transport: &Transport) -> Result<HttpsClient, String> {
    // rustls needs a crypto provider chosen before any TLS config is built. aws-lc-rs
    // uses the CPU's SHA and AES instructions, which matters when signing and
    // transferring at multiple gigabits.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.set_connect_timeout(Some(Duration::from_secs(transport.connect_timeout.unwrap_or(10))));
    // Without this, small requests wait on Nagle's algorithm for an ack that the peer is
    // delaying — tens of milliseconds added to every round trip.
    http.set_nodelay(true);
    http.enforce_http(false);

    // The common case keeps hyper-rustls's own native-root loading. `--ca-bundle` and
    // `--no-verify-ssl` both need a hand-built config, so they take the other branch.
    let tls = if transport.verify_ssl && transport.ca_bundle.is_none() {
        hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|e| format!("could not load the system certificate store: {e}"))?
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(http)
    } else {
        hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_config(transport)?)
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(http)
    };

    Ok(Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build(tls))
}

/// The TLS configuration for `--ca-bundle` and `--no-verify-ssl`.
fn tls_config(transport: &Transport) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    // With no bundle the store stays empty, which is only reachable under
    // `--no-verify-ssl`, where the verifier never consults it.
    if let Some(path) = &transport.ca_bundle {
        let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut reader = std::io::BufReader::new(file);
        let mut added = 0usize;
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.map_err(|e| format!("{path}: {e}"))?;
            roots.add(cert).map_err(|e| format!("{path}: {e}"))?;
            added += 1;
        }
        // An empty or non-PEM file would otherwise produce a store that trusts nothing,
        // and every request would fail with an opaque certificate error rather than
        // naming the real problem.
        if added == 0 {
            return Err(format!("{path}: no PEM certificates found"));
        }
    }

    // Named explicitly rather than relying on the process-wide default having been
    // installed already: that only holds because `build_client` installs it a few lines
    // earlier, which is an ordering dependency nothing states.
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("could not build a TLS configuration: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();

    if !transport.verify_ssl {
        config.dangerous().set_certificate_verifier(std::sync::Arc::new(NoVerification(provider)));
    }
    Ok(config)
}

/// The `--no-verify-ssl` certificate verifier: accepts any chain.
///
/// Signature checking is left intact — only the identity of the peer goes unverified,
/// which is what the flag asks for. This makes the connection trivially interceptable,
/// so it exists for talking to test endpoints with self-signed certificates and nothing
/// else.
#[derive(Debug)]
struct NoVerification(std::sync::Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A byte stream whose exact length is known up front.
///
/// This exists because of a silent-corruption bug worth remembering: a body with an
/// unknown size hint makes hyper fall back to `Transfer-Encoding: chunked` and omit
/// `Content-Length`. S3 requires `Content-Length` on `PutObject` and `UploadPart`, so
/// every streamed upload stored a zero-byte object while the transfer still reported
/// success. Reporting the exact size keeps the framing correct.
struct SizedStream {
    inner: std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<Bytes, std::io::Error>> + Send + Sync>>,
    len: u64,
}

impl hyper::body::Body for SizedStream {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.inner.as_mut().poll_next(cx).map(|next| next.map(|chunk| chunk.map(Frame::data)))
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(self.len)
    }
}

/// Turn a [`Body`] into something hyper can send, streaming file-backed bodies rather
/// than reading them into memory.
async fn into_hyper_body(body: Body) -> Result<BoxBody<Bytes, std::io::Error>, RuntimeError> {
    match body {
        Body::Empty => Ok(Full::new(Bytes::new())
            .map_err(|e: std::convert::Infallible| match e {})
            .boxed()),
        Body::Bytes(b) => Ok(Full::new(b)
            .map_err(|e: std::convert::Infallible| match e {})
            .boxed()),
        Body::FileRange { path, offset, len } => {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| RuntimeError::Http(format!("{}: {e}", path.display())))?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| RuntimeError::Http(format!("{}: {e}", path.display())))?;
            let stream = tokio_util::io::ReaderStream::with_capacity(file.take(len), 64 * 1024);
            Ok(SizedStream { inner: Box::pin(stream), len }.boxed())
        }
    }
}

fn build_hyper_request(
    req: &Request,
    body: BoxBody<Bytes, std::io::Error>,
) -> Result<hyper::Request<BoxBody<Bytes, std::io::Error>>, RuntimeError> {
    let mut builder = hyper::Request::builder()
        .method(req.method.as_str())
        .uri(req.url.as_str());
    for (k, v) in &req.headers {
        // `host` is derived from the URI by the transport; sending it explicitly would
        // duplicate the header.
        if !k.eq_ignore_ascii_case("host") {
            builder = builder.header(k.as_str(), v.as_str());
        }
    }
    builder.body(body).map_err(|e| RuntimeError::Http(e.to_string()))
}

/// A connection error with its cause chain.
///
/// hyper's own `Display` for a connect failure is just `client error (Connect)`, which
/// hides the thing the user needs — the certificate was rejected, the name did not
/// resolve, the port refused. The cause is always one or two `source()` links down.
fn describe(error: &hyper_util::client::legacy::Error) -> String {
    use std::error::Error;
    let mut message = error.to_string();
    let mut source: Option<&(dyn Error + 'static)> = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

fn collect_headers(parts: &hyper::http::response::Parts) -> Vec<(String, String)> {
    parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect()
}

/// Send a request and read the whole response body into memory.
pub fn send(req: &Request, transport: &Transport) -> Result<Response, RuntimeError> {
    runtime().block_on(send_async(req, transport))
}

pub async fn send_async(req: &Request, transport: &Transport) -> Result<Response, RuntimeError> {
    let client = client(transport)?;
    let hyper_body = into_hyper_body(req.body.clone()).await?;
    let request = build_hyper_request(req, hyper_body)?;

    let response = tokio::time::timeout(transport.read_timeout(), client.request(request))
        .await
        .map_err(|_| RuntimeError::Http("request timed out".to_string()))?
        .map_err(|e| RuntimeError::Http(describe(&e)))?;

    let (parts, body) = response.into_parts();
    let headers = collect_headers(&parts);
    let status = parts.status.as_u16();

    // A HEAD response is not special-cased. It advertises the Content-Length of the body
    // it describes but sends none, and hyper knows that, so the frame loop below ends
    // immediately. Returning early instead would drop the body without polling it, and a
    // body that is never driven to completion is a connection the pool cannot reuse —
    // which silently undoes the pooling for any workload that heads before it gets.
    //
    // Per frame, not around the whole body: a read timeout means "the peer went quiet",
    // not "the transfer must finish within 60 seconds". Timing the collection as a whole
    // would abort any download that legitimately takes longer than one timeout.
    let mut body = body;
    let mut collected = Vec::new();
    while let Some(frame) = tokio::time::timeout(transport.read_timeout(), body.frame())
        .await
        .map_err(|_| RuntimeError::Http("request timed out".to_string()))?
    {
        let frame = frame.map_err(|e| RuntimeError::Http(e.to_string()))?;
        if let Ok(chunk) = frame.into_data() {
            collected.extend_from_slice(&chunk);
        }
    }

    Ok(Response { status, body: Bytes::from(collected), headers })
}

/// Send a request and stream the response body into `sink` as it arrives.
///
/// This is the download path. Nothing larger than one chunk is ever held in memory, so
/// the peak cost of fetching a 5 GB object is a few tens of kilobytes.
///
/// An error status is *not* streamed: the body is a short error document the caller needs
/// to parse, so it comes back in [`Err`] as text.
pub fn send_to_writer<W: std::io::Write>(
    req: &Request,
    transport: &Transport,
    sink: &mut W,
) -> Result<ResponseHead, RuntimeError> {
    runtime().block_on(async {
            let client = client(transport)?;

        let hyper_body = into_hyper_body(req.body.clone()).await?;
        let request = build_hyper_request(req, hyper_body)?;

        let response = tokio::time::timeout(transport.read_timeout(), client.request(request))
            .await
            .map_err(|_| RuntimeError::Http("request timed out".to_string()))?
            .map_err(|e| RuntimeError::Http(describe(&e)))?;

        let (parts, mut body) = response.into_parts();
        let headers = collect_headers(&parts);
        let status = parts.status.as_u16();

        if status >= 400 {
            let collected = body.collect().await.map_err(|e| RuntimeError::Http(e.to_string()))?;
            return Err(RuntimeError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&collected.to_bytes()).into_owned(),
                headers,
            });
        }

        while let Some(frame) = tokio::time::timeout(transport.read_timeout(), body.frame())
            .await
            .map_err(|_| RuntimeError::Http("request timed out".to_string()))?
        {
            let frame = frame.map_err(|e| RuntimeError::Http(e.to_string()))?;
            if let Ok(chunk) = frame.into_data() {
                sink.write_all(&chunk).map_err(|e| RuntimeError::Http(e.to_string()))?;
            }
        }

        Ok(ResponseHead { status, headers })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bundle(path: &str) -> Result<rustls::ClientConfig, String> {
        tls_config(&Transport {
            verify_ssl: true,
            ca_bundle: Some(path.to_string()),
            ..Transport::default()
        })
    }

    #[test]
    fn a_missing_ca_bundle_names_the_file() {
        let error = with_bundle("/nonexistent/ca.pem").unwrap_err();
        assert!(error.contains("/nonexistent/ca.pem"), "{error}");
    }

    /// A bundle that parses to nothing would otherwise build a store trusting nobody,
    /// and every request would fail with an opaque certificate error instead.
    #[test]
    fn a_ca_bundle_with_no_certificates_is_rejected() {
        let path = std::env::temp_dir().join("awsc-empty-bundle.pem");
        std::fs::write(&path, b"not a certificate\n").expect("write fixture");
        let error = with_bundle(path.to_str().expect("utf-8 path")).unwrap_err();
        assert!(error.contains("no PEM certificates found"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    /// `--no-verify-ssl` must not need a bundle to build a usable config.
    #[test]
    fn no_verify_ssl_builds_without_roots() {
        let config =
            tls_config(&Transport { verify_ssl: false, ..Transport::default() });
        assert!(config.is_ok());
    }
}
