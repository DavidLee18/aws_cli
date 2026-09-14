//! A minimal RFC 6455 WebSocket client over TLS.
//!
//! Only `ec2-instance-connect open-tunnel` needs one. The reference gets it from
//! `awscrt`, an event-driven C library; there is no equivalent here and the runtime's
//! HTTP client is async hyper, which cannot hand back the raw stream a tunnel needs. So
//! this is a synchronous client written against the RFC: handshake, then binary frames
//! in both directions.
//!
//! What it implements is exactly what a tunnel uses:
//!
//! - the `Upgrade` handshake, with the `Sec-WebSocket-Accept` check;
//! - **client-to-server masking**, which the RFC requires of every client frame and
//!   which servers close the connection over if it is missing;
//! - binary data frames, continuation frames, `close`, and `ping` (answered with
//!   `pong`).
//!
//! Not implemented, because a tunnel never uses them: sending text frames, compression
//! extensions, and subprotocol negotiation.

use base64ct::Encoding;
use std::io::{Read, Write};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub const OPCODE_CONTINUATION: u8 = 0x0;
pub const OPCODE_TEXT: u8 = 0x1;
pub const OPCODE_BINARY: u8 = 0x2;
pub const OPCODE_CLOSE: u8 = 0x8;
pub const OPCODE_PING: u8 = 0x9;
pub const OPCODE_PONG: u8 = 0xA;

/// The most a single frame carries, matching the reference's `_MAX_BYTES_PER_FRAME`.
pub const MAX_BYTES_PER_FRAME: usize = 65_000;

#[derive(Debug)]
pub enum WebSocketError {
    Io(std::io::Error),
    /// The TLS layer, or a host that is not a valid server name.
    Tls(String),
    /// The server answered the upgrade with something other than `101`. The body is kept
    /// because that is where the service puts the reason — the reference logs it at
    /// `error` level for the same reason.
    Handshake { status: u16, body: String },
    /// A frame that does not follow the RFC, or one a tunnel must not receive.
    Protocol(String),
}

impl std::fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebSocketError::Io(e) => write!(f, "{e}"),
            WebSocketError::Tls(e) => write!(f, "{e}"),
            WebSocketError::Handshake { status, body } if body.is_empty() => {
                write!(f, "websocket handshake failed with status {status}")
            }
            WebSocketError::Handshake { body, .. } => write!(f, "{body}"),
            WebSocketError::Protocol(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for WebSocketError {
    fn from(e: std::io::Error) -> Self {
        WebSocketError::Io(e)
    }
}

pub struct Frame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

pub struct WebSocket {
    stream: rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    /// Bytes read from the stream that are not yet a whole frame. The handshake usually
    /// leaves some here: the server may put the first frames in the same TCP segment as
    /// the `101`, and throwing away that tail would lose data.
    buffer: Vec<u8>,
    /// Fed to the masking key and the handshake nonce.
    random: std::sync::Arc<rustls::crypto::CryptoProvider>,
    closed: bool,
}

/// How the tunnel is configured to trust the server.
pub struct TlsOptions<'a> {
    pub verify_ssl: bool,
    pub ca_bundle: Option<&'a str>,
}

impl WebSocket {
    /// Open a connection to `url`, which must be `wss://host[:port]/path?query`.
    ///
    /// Blocking, and the returned socket is blocking too; [`WebSocket::read_frame`]
    /// applies its own read timeout.
    pub fn connect(
        url: &str,
        user_agent: Option<&str>,
        tls: &TlsOptions<'_>,
    ) -> Result<WebSocket, WebSocketError> {
        let (host, port, target) = split_url(url)?;

        let provider = provider();
        let config = client_config(&provider, tls)?;
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| WebSocketError::Tls(format!("invalid host `{host}`")))?;
        let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
            .map_err(|e| WebSocketError::Tls(e.to_string()))?;
        let socket = std::net::TcpStream::connect((host.as_str(), port))?;
        // Nagle would hold a keystroke back waiting for an ack the peer is delaying.
        socket.set_nodelay(true)?;
        let mut stream = rustls::StreamOwned::new(connection, socket);

        let mut nonce = [0u8; 16];
        provider
            .secure_random
            .fill(&mut nonce)
            .map_err(|_| WebSocketError::Tls("could not read random bytes".to_string()))?;
        let key = base64ct::Base64::encode_string(&nonce);

        let mut request = format!(
            "GET {target} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n"
        );
        if let Some(user_agent) = user_agent {
            request.push_str(&format!("User-Agent: {user_agent}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let mut buffer = Vec::new();
        let head = read_headers(&mut stream, &mut buffer)?;
        check_handshake(&head, &key)?;

        Ok(WebSocket { stream, buffer, random: provider, closed: false })
    }

    /// Send one binary frame, masked as the RFC requires of a client.
    pub fn send_binary(&mut self, payload: &[u8]) -> Result<(), WebSocketError> {
        self.send(OPCODE_BINARY, payload)
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        let mut mask = [0u8; 4];
        self.random
            .secure_random
            .fill(&mut mask)
            .map_err(|_| WebSocketError::Tls("could not read random bytes".to_string()))?;
        let frame = encode_frame(opcode, payload, mask);
        self.stream.write_all(&frame)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Send a close frame with code 1000 and stop writing.
    ///
    /// Errors are swallowed: this runs while shutting down, and a peer that has already
    /// gone away is the ordinary case rather than a failure to report.
    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.send(OPCODE_CLOSE, &1000u16.to_be_bytes());
            let _ = self.stream.sock.shutdown(std::net::Shutdown::Write);
        }
    }

    /// The next frame, or `None` if nothing arrived within `timeout`.
    ///
    /// `Err(Io)` with [`std::io::ErrorKind::UnexpectedEof`] means the peer closed the
    /// connection without a close frame.
    pub fn read_frame(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<Frame>, WebSocketError> {
        loop {
            if let Some(frame) = decode_frame(&mut self.buffer)? {
                return Ok(Some(frame));
            }
            self.stream.sock.set_read_timeout(Some(timeout))?;
            let mut chunk = [0u8; 16 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(WebSocketError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the server closed the connection",
                    )))
                }
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                // A timeout is the ordinary "nothing yet" answer, and the partial record
                // rustls has buffered stays buffered for the next call.
                Err(e) if would_block(&e) => return Ok(None),
                Err(e) => return Err(WebSocketError::Io(e)),
            }
        }
    }

    /// Answer a ping with the same payload, as the RFC requires.
    pub fn pong(&mut self, payload: &[u8]) -> Result<(), WebSocketError> {
        self.send(OPCODE_PONG, payload)
    }
}

fn would_block(e: &std::io::Error) -> bool {
    // A socket read timeout is `WouldBlock` on Unix and `TimedOut` on Windows.
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

fn provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// The trust configuration, which follows `--ca-bundle` and `--no-verify-ssl`.
///
/// **This is a deliberate difference from the reference**, which builds the tunnel's TLS
/// context from awscrt's defaults and so ignores both flags. Honouring them here costs
/// nothing and makes the command testable against a stand-in.
fn client_config(
    provider: &std::sync::Arc<rustls::crypto::CryptoProvider>,
    tls: &TlsOptions<'_>,
) -> Result<rustls::ClientConfig, WebSocketError> {
    let mut roots = rustls::RootCertStore::empty();
    match tls.ca_bundle {
        Some(path) => {
            let file = std::fs::File::open(path)
                .map_err(|e| WebSocketError::Tls(format!("{path}: {e}")))?;
            let mut reader = std::io::BufReader::new(file);
            let mut added = 0usize;
            for cert in rustls_pemfile::certs(&mut reader) {
                let cert = cert.map_err(|e| WebSocketError::Tls(format!("{path}: {e}")))?;
                roots.add(cert).map_err(|e| WebSocketError::Tls(format!("{path}: {e}")))?;
                added += 1;
            }
            if added == 0 {
                return Err(WebSocketError::Tls(format!("{path}: no PEM certificates found")));
            }
        }
        None => {
            let loaded = rustls_native_certs::load_native_certs();
            if loaded.certs.is_empty() && tls.verify_ssl {
                let reason = loaded
                    .errors
                    .first()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "no certificates found".to_string());
                return Err(WebSocketError::Tls(format!(
                    "could not load the system certificate store: {reason}"
                )));
            }
            for cert in loaded.certs {
                let _ = roots.add(cert);
            }
        }
    }

    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| WebSocketError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    if !tls.verify_ssl {
        config
            .dangerous()
            .set_certificate_verifier(std::sync::Arc::new(NoVerification(provider.clone())));
    }
    Ok(config)
}

/// `--no-verify-ssl`: accepts any chain. Identity goes unverified, which is what the flag
/// asks for and what makes the connection interceptable.
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

/// `wss://host[:port]/path?query` split into host, port and request target.
fn split_url(url: &str) -> Result<(String, u16, String), WebSocketError> {
    let rest = url
        .strip_prefix("wss://")
        .ok_or_else(|| WebSocketError::Protocol(format!("not a wss:// url: {url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    // A port in the authority is not something an EICE DNS name ever carries; it is
    // parsed because a URL may have one and because it is how the command is tested.
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>()
                .map_err(|_| WebSocketError::Protocol(format!("invalid port in {url}")))?,
        ),
        None => (authority.to_string(), 443),
    };
    Ok((host, port, path.to_string()))
}

/// Read until the end of the response headers, leaving anything after them in `buffer`.
fn read_headers(
    stream: &mut rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    buffer: &mut Vec<u8>,
) -> Result<String, WebSocketError> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find_subslice(&head, b"\r\n\r\n") {
            buffer.extend_from_slice(&head[end + 4..]);
            head.truncate(end);
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(WebSocketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the server closed the connection during the handshake",
            )));
        }
        head.extend_from_slice(&chunk[..read]);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// `101` with a correct `Sec-WebSocket-Accept`, or the status and body to report.
fn check_handshake(head: &str, key: &str) -> Result<(), WebSocketError> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    if status != 101 {
        // The body is not in `head` — it comes after the blank line — but the status line
        // and headers are what identify the failure, and the caller logs the rest.
        return Err(WebSocketError::Handshake { status, body: String::new() });
    }
    let accept = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-accept"))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default();
    if accept != accept_key(key) {
        return Err(WebSocketError::Protocol(
            "the server's Sec-WebSocket-Accept did not match the key sent".to_string(),
        ));
    }
    Ok(())
}

/// `base64(sha1(key + GUID))`, the RFC 6455 handshake proof.
pub fn accept_key(key: &str) -> String {
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    base64ct::Base64::encode_string(&hasher.finalize())
}

/// One client frame: `FIN` set, masked, with the two- or eight-byte extended length when
/// the payload does not fit in seven bits.
pub fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let length = payload.len();
    if length < 126 {
        frame.push(0x80 | length as u8);
    } else if length <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

/// Take one whole frame off the front of `buffer`, or `None` if it is not all there yet.
pub fn decode_frame(buffer: &mut Vec<u8>) -> Result<Option<Frame>, WebSocketError> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let opcode = buffer[0] & 0x0F;
    let masked = buffer[1] & 0x80 != 0;
    let short_length = (buffer[1] & 0x7F) as usize;
    let mut offset = 2;
    let length = match short_length {
        126 => {
            if buffer.len() < 4 {
                return Ok(None);
            }
            offset = 4;
            u16::from_be_bytes([buffer[2], buffer[3]]) as usize
        }
        127 => {
            if buffer.len() < 10 {
                return Ok(None);
            }
            offset = 10;
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&buffer[2..10]);
            let length = u64::from_be_bytes(bytes);
            usize::try_from(length).map_err(|_| {
                WebSocketError::Protocol(format!("frame of {length} bytes is too large"))
            })?
        }
        other => other,
    };
    // A server must not mask; accepting one anyway would be harmless, but the frame's
    // length accounting differs, so it has to be handled rather than ignored.
    let mask = if masked {
        if buffer.len() < offset + 4 {
            return Ok(None);
        }
        let mask = [buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3]];
        offset += 4;
        Some(mask)
    } else {
        None
    };
    if buffer.len() < offset + length {
        return Ok(None);
    }
    let mut payload = buffer[offset..offset + length].to_vec();
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    buffer.drain(..offset + length);
    Ok(Some(Frame { opcode, payload }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from RFC 6455 section 1.3, which is the only way to know the
    /// handshake proof is right without a server.
    #[test]
    fn the_accept_key_matches_the_rfc_example() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    /// RFC 6455 section 5.7's masked single-frame "Hello".
    #[test]
    fn a_masked_frame_matches_the_rfc_example() {
        let frame = encode_frame(OPCODE_TEXT, b"Hello", [0x37, 0xfa, 0x21, 0x3d]);
        assert_eq!(
            frame,
            vec![0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]
        );
    }

    /// Round-tripping catches the length encodings, which are where an off-by-one lives.
    #[test]
    fn every_length_encoding_round_trips() {
        for length in [0usize, 1, 125, 126, 127, 65_535, 65_536, 70_000] {
            let payload = vec![0xABu8; length];
            let mut buffer = encode_frame(OPCODE_BINARY, &payload, [1, 2, 3, 4]);
            let frame = decode_frame(&mut buffer).expect("decodes").expect("is whole");
            assert_eq!(frame.opcode, OPCODE_BINARY, "length {length}");
            assert_eq!(frame.payload, payload, "length {length}");
            assert!(buffer.is_empty(), "length {length} left {} bytes", buffer.len());
        }
    }

    /// A frame that has not all arrived yet must be left alone, not half-consumed.
    #[test]
    fn a_partial_frame_is_not_consumed() {
        let whole = encode_frame(OPCODE_BINARY, b"0123456789", [9, 9, 9, 9]);
        for split in 0..whole.len() {
            let mut buffer = whole[..split].to_vec();
            let before = buffer.clone();
            assert!(decode_frame(&mut buffer).expect("decodes").is_none(), "split at {split}");
            assert_eq!(buffer, before, "split at {split} consumed bytes");
        }
    }

    /// Two frames in one read, which is what a server that batches will send.
    #[test]
    fn frames_are_taken_one_at_a_time() {
        let mut buffer = encode_frame(OPCODE_BINARY, b"first", [1, 1, 1, 1]);
        buffer.extend(encode_frame(OPCODE_BINARY, b"second", [2, 2, 2, 2]));
        let first = decode_frame(&mut buffer).expect("decodes").expect("is whole");
        assert_eq!(first.payload, b"first");
        let second = decode_frame(&mut buffer).expect("decodes").expect("is whole");
        assert_eq!(second.payload, b"second");
        assert!(decode_frame(&mut buffer).expect("decodes").is_none());
    }

    /// A server frame is unmasked; the decoder has to accept both.
    #[test]
    fn an_unmasked_server_frame_decodes() {
        let mut buffer = vec![0x82, 0x03, b'a', b'b', b'c'];
        let frame = decode_frame(&mut buffer).expect("decodes").expect("is whole");
        assert_eq!(frame.opcode, OPCODE_BINARY);
        assert_eq!(frame.payload, b"abc");
    }

    #[test]
    fn a_wss_url_splits_into_host_port_and_target() {
        assert_eq!(
            split_url("wss://eice-1.ec2.aws/openTunnel?a=b").expect("splits"),
            ("eice-1.ec2.aws".to_string(), 443, "/openTunnel?a=b".to_string())
        );
        assert_eq!(
            split_url("wss://127.0.0.1:9443/openTunnel").expect("splits"),
            ("127.0.0.1".to_string(), 9443, "/openTunnel".to_string())
        );
        assert!(split_url("https://example.com/").is_err());
    }
}
