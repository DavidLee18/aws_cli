//! `aws ec2-instance-connect open-tunnel`.
//!
//! Port of `customizations/ec2instanceconnect/`. The command opens a WebSocket to an EC2
//! Instance Connect Endpoint and proxies bytes between it and either stdin/stdout or a
//! local TCP listener. In practice it is what `ProxyCommand` in an SSH config runs.
//!
//! Three things are worth knowing before reading it:
//!
//! - **The tunnel URL is a presigned SigV4 URL**, signed for `ec2-instance-connect` with
//!   a 60-second expiry, and a *fresh one is signed per connection* — in listener mode a
//!   long-lived listener keeps working because nothing reuses an old signature.
//! - **The endpoint is discovered, not configured.** Given only an instance id, the
//!   command reads the instance's VPC and subnet, lists the endpoints in that VPC, and
//!   prefers one in the same subnet — falling back to any endpoint in the VPC.
//! - **stdin/stdout mode refuses to run on a terminal.** It is a byte pipe; a human
//!   typing at it would see binary and corrupt the stream.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::custom::{resolve_region, take_args};
use crate::exit;
use crate::websocket::{self, TlsOptions, WebSocket};
use crate::Failure;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::ExitCode;

/// How long the main loop waits on the socket before checking for local input.
///
/// It bounds the latency this adds to a keystroke. The reference is event-driven on the
/// websocket side and polls stdin every 50 ms; this polls the other way around, so the
/// interval is shorter to keep an interactive session feeling the same.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// The endpoint states that can carry a tunnel.
const USABLE_STATES: [&str; 4] =
    ["create-complete", "update-in-progress", "update-failed", "update-complete"];

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "open-tunnel" => open_tunnel(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

fn param_error(message: &str) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(message.to_string()),
    )
}

struct Options {
    remote_port: u32,
    local_port: Option<u16>,
    max_tunnel_duration: Option<u32>,
    max_websocket_connections: usize,
}

fn open_tunnel(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(
        parsed,
        &[
            "--instance-id",
            "--instance-connect-endpoint-id",
            "--instance-connect-endpoint-dns-name",
            "--private-ip-address",
            "--remote-port",
            "--local-port",
            "--max-tunnel-duration",
            "--max-websocket-connections",
        ],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let number = |flag: &str| -> Result<Option<u32>, Failure> {
        match value(flag) {
            None => Ok(None),
            Some(text) => text.parse::<u32>().map(Some).map_err(|_| {
                param_error(&format!("Invalid value for {flag}: {text}"))
            }),
        }
    };

    let instance_id = value("--instance-id");
    let mut endpoint_id = value("--instance-connect-endpoint-id").map(str::to_string);
    let mut dns_name = value("--instance-connect-endpoint-dns-name").map(str::to_string);
    let mut private_ip = value("--private-ip-address").map(str::to_string);
    let options = Options {
        remote_port: number("--remote-port")?.unwrap_or(22),
        local_port: number("--local-port")?
            .map(|port| {
                u16::try_from(port)
                    .map_err(|_| param_error(&format!("Invalid value for --local-port: {port}")))
            })
            .transpose()?,
        max_tunnel_duration: number("--max-tunnel-duration")?,
        max_websocket_connections: number("--max-websocket-connections")?.unwrap_or(10) as usize,
    };

    // The reference's order, and the wording is its wording — including the trailing
    // space on the last one.
    if instance_id.is_none() && private_ip.is_none() {
        return Err(param_error("Specify an instance id or private ip."));
    }
    if dns_name.is_some() && endpoint_id.is_none() {
        return Err(param_error(
            "Specify an instance connect endpoint id when providing a DNS name.",
        ));
    }
    if private_ip.is_some() && endpoint_id.is_none() {
        return Err(param_error(
            "Specify an instance connect endpoint id when providing a private ip.",
        ));
    }
    if let Some(duration) = options.max_tunnel_duration {
        if !(1..=3_600).contains(&duration) {
            return Err(param_error(
                "Invalid max connection timeout specified. Value must be greater than 1 and \
                 less than 3600.",
            ));
        }
    }
    if options.local_port.is_none() && stdin_is_a_terminal() {
        return Err(param_error(
            "This command does not support interactive mode. You must use this command as a \
             proxy or in listener mode. ",
        ));
    }

    let region = resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    // `--endpoint-url` does not follow: the reference builds the EC2 client without it.
    let ec2_globals = Globals { region: Some(region.clone()), ..globals.for_service("ec2") };
    let ec2_model = crate::load_model("ec2").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let ec2 = Client::new(&ec2_model, &ec2_globals)?;

    let mut vpc_id = None;
    let mut subnet_id = None;
    if private_ip.is_none() {
        let described = ec2.call(
            "describe-instances",
            Some(&json!({ "InstanceIds": [instance_id.unwrap_or_default()] })),
        )?;
        let instance = described
            .get("Reservations")
            .and_then(|r| r.get(0))
            .and_then(|r| r.get("Instances"))
            .and_then(|i| i.get(0))
            .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "list index out of range"))?;
        vpc_id = instance.get("VpcId").and_then(Value::as_str).map(str::to_string);
        subnet_id = instance.get("SubnetId").and_then(Value::as_str).map(str::to_string);
        // IPv6 is the fallback: an instance in an IPv6-only subnet has no private IPv4,
        // and the endpoint takes either.
        private_ip = instance
            .get("PrivateIpAddress")
            .and_then(Value::as_str)
            .or_else(|| instance.get("Ipv6Address").and_then(Value::as_str))
            .map(str::to_string);
        if private_ip.is_none() {
            return Err(param_error(
                "Unable to find any IP address on the instance to connect to.",
            ));
        }
    }

    if dns_name.is_none() {
        let endpoint = find_endpoint(
            &ec2,
            vpc_id.as_deref(),
            subnet_id.as_deref(),
            endpoint_id.as_deref(),
        )?;
        endpoint_id = endpoint
            .get("InstanceConnectEndpointId")
            .and_then(Value::as_str)
            .map(str::to_string);
        dns_name = Some(eice_dns_name(&endpoint, use_fips_endpoint(globals))?);
    }

    let signer = Signer {
        credentials: ec2.credentials.clone(),
        region,
        dns_name: dns_name.unwrap_or_default(),
        endpoint_id: endpoint_id.unwrap_or_default(),
        remote_port: options.remote_port,
        private_ip: private_ip.unwrap_or_default(),
        max_tunnel_duration: options.max_tunnel_duration,
    };
    let tls = TlsOptions { verify_ssl: globals.verify_ssl, ca_bundle: globals.ca_bundle.as_deref() };

    match options.local_port {
        None => {
            let outcome = run_one(&signer, &tls, Endpoints::Stdio, None);
            report(outcome, None)
        }
        Some(port) => listen(&signer, &tls, port, options.max_websocket_connections),
    }
}

/// Report a connection's outcome the way the reference does, and choose the exit code.
fn report(outcome: Result<(), String>, id: Option<u64>) -> Result<ExitCode, Failure> {
    match outcome {
        Ok(()) => Ok(exit::code(exit::SUCCESS)),
        Err(message) => match id {
            Some(id) => {
                eprintln!("[{id}] Encountered error with websocket: {message}");
                Ok(exit::code(exit::GENERAL_ERROR))
            }
            None => Err(Failure::new(exit::GENERAL_ERROR, message)),
        },
    }
}

/// Listener mode: one websocket per accepted TCP connection.
fn listen(
    signer: &Signer,
    tls: &TlsOptions<'_>,
    port: u16,
    max_connections: usize,
) -> Result<ExitCode, Failure> {
    let listener = std::net::TcpListener::bind(("localhost", port))
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{e}")))?;
    println!("Listening for connections on port {port}.");
    let _ = std::io::stdout().flush();

    let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut next_id = 1u64;
    for connection in listener.incoming() {
        let connection = match connection {
            Ok(connection) => connection,
            Err(_) => continue,
        };
        if live.load(std::sync::atomic::Ordering::SeqCst) >= max_connections {
            println!(
                "Max websocket connections {max_connections} have been reached, closing \
                 incoming connection."
            );
            let _ = std::io::stdout().flush();
            drop(connection);
            continue;
        }
        let id = next_id;
        next_id += 1;
        println!("[{id}] Accepted new tcp connection, opening websocket tunnel.");
        let _ = std::io::stdout().flush();

        // Each connection signs its own URL: a signature is good for 60 seconds, so a
        // listener that stayed up for an hour could not reuse the first one.
        let signer = signer.clone();
        let verify_ssl = tls.verify_ssl;
        let ca_bundle = tls.ca_bundle.map(str::to_string);
        let live = live.clone();
        live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::spawn(move || {
            let tls =
                TlsOptions { verify_ssl, ca_bundle: ca_bundle.as_deref() };
            let outcome = run_one(&signer, &tls, Endpoints::Tcp(connection), Some(id));
            if let Err(message) = outcome {
                eprintln!("[{id}] Encountered error with websocket: {message}");
            }
            println!("[{id}] Closing tcp connection.");
            let _ = std::io::stdout().flush();
            live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    Ok(exit::code(exit::SUCCESS))
}

/// Where the local end of one tunnel is.
enum Endpoints {
    Stdio,
    Tcp(std::net::TcpStream),
}

impl Endpoints {
    /// Split into the half that is read from and the half that is written to, so the
    /// reader can block in its own thread while the main loop writes.
    fn split(self) -> std::io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        match self {
            Endpoints::Stdio => {
                Ok((Box::new(std::io::stdin()), Box::new(std::io::stdout())))
            }
            Endpoints::Tcp(stream) => {
                let write_half = stream.try_clone()?;
                Ok((Box::new(stream), Box::new(write_half)))
            }
        }
    }
}

/// Open one websocket and pump bytes until either end closes.
fn run_one(
    signer: &Signer,
    tls: &TlsOptions<'_>,
    endpoints: Endpoints,
    id: Option<u64>,
) -> Result<(), String> {
    let url = signer.presigned_url();
    let mut socket = WebSocket::connect(&url, Some(&awsc_runtime::http::user_agent()), tls).map_err(|e| {
        if let websocket::WebSocketError::Handshake { status, .. } = &e {
            format!("the endpoint refused the tunnel (HTTP {status})")
        } else {
            e.to_string()
        }
    })?;
    let (mut input, mut output) = endpoints.split().map_err(|e| e.to_string())?;

    // The local side is read in its own thread: reading stdin or a socket blocks, and
    // the main loop has to stay free to deliver what arrives from the tunnel.
    let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; websocket::MAX_BYTES_PER_FRAME];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if sender.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let result = pump(&mut socket, &mut output, &receiver);
    socket.close();
    let _ = id;
    result
}

fn pump(
    socket: &mut WebSocket,
    output: &mut Box<dyn Write + Send>,
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    let mut input_open = true;
    loop {
        // Everything waiting from the local side goes out first, so a burst leaves in as
        // few round trips as it arrived.
        loop {
            match receiver.try_recv() {
                Ok(data) => socket.send_binary(&data).map_err(|e| e.to_string())?,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The local side is done sending. Half-close rather than tearing the
                // tunnel down: whatever the server has already put on the wire is still
                // owed to the local end, and dropping it here would truncate the last
                // reply of a session. The server answers a close frame with its own,
                // which is what ends the loop below.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if input_open {
                        input_open = false;
                        socket.close();
                    }
                    break;
                }
            }
        }

        let frame = match socket.read_frame(POLL_INTERVAL) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            // A peer that vanished without a close frame is an ordinary end of session.
            Err(websocket::WebSocketError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(())
            }
            Err(e) => return Err(e.to_string()),
        };
        match frame.opcode {
            websocket::OPCODE_BINARY | websocket::OPCODE_CONTINUATION => {
                output.write_all(&frame.payload).map_err(|e| e.to_string())?;
                output.flush().map_err(|e| e.to_string())?;
            }
            // A tunnel is binary. Text means the server is saying something this is not
            // equipped to interpret, and passing it through would corrupt the stream.
            websocket::OPCODE_TEXT => {
                return Err(
                    "Received invalid data from server, closing websocket connection."
                        .to_string(),
                )
            }
            websocket::OPCODE_PING => socket.pong(&frame.payload).map_err(|e| e.to_string())?,
            websocket::OPCODE_PONG => {}
            websocket::OPCODE_CLOSE => return closure_reason(&frame.payload),
            other => return Err(format!("unexpected websocket opcode {other:#x}")),
        }
    }
}

/// A close frame carries a two-byte code and a reason. Anything but 1000 is reported.
fn closure_reason(payload: &[u8]) -> Result<(), String> {
    if payload.len() < 2 {
        return Ok(());
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if code == 1000 {
        return Ok(());
    }
    Err(format!(
        "Websocket Closure Reason: {}",
        String::from_utf8_lossy(&payload[2..])
    ))
}

#[derive(Clone)]
struct Signer {
    credentials: awsc_runtime::credentials::Credentials,
    region: String,
    dns_name: String,
    endpoint_id: String,
    remote_port: u32,
    private_ip: String,
    max_tunnel_duration: Option<u32>,
}

impl Signer {
    /// The `wss://` URL, presigned for `ec2-instance-connect` and good for 60 seconds.
    fn presigned_url(&self) -> String {
        let mut params = vec![
            ("instanceConnectEndpointId".to_string(), self.endpoint_id.clone()),
            ("remotePort".to_string(), self.remote_port.to_string()),
            ("privateIpAddress".to_string(), self.private_ip.clone()),
        ];
        if let Some(duration) = self.max_tunnel_duration {
            params.push(("maxTunnelDuration".to_string(), duration.to_string()));
        }
        let timestamp = awsc_runtime::sigv4::format_timestamp(crate::now_unix());
        let ctx = awsc_runtime::sigv4::SigningContext {
            credentials: &self.credentials,
            region: &self.region,
            service: "ec2-instance-connect",
            timestamp: &timestamp,
        };
        let query = awsc_runtime::presign::presign(
            &ctx,
            &awsc_runtime::presign::PresignRequest {
                method: "GET",
                host: &self.dns_name,
                path: "/openTunnel",
                params,
                extra_signed_headers: vec![],
                expires: 60,
                payload: awsc_runtime::presign::Payload::EmptyBody,
            },
        );
        format!("wss://{}/openTunnel?{query}", self.dns_name)
    }
}

/// The endpoint to tunnel through: the named one, or the best one in the instance's VPC.
fn find_endpoint(
    ec2: &Client<'_>,
    vpc_id: Option<&str>,
    subnet_id: Option<&str>,
    endpoint_id: Option<&str>,
) -> Result<Value, Failure> {
    let state_filter = json!({ "Name": "state", "Values": USABLE_STATES });
    if let Some(endpoint_id) = endpoint_id {
        let described = ec2.call(
            "describe-instance-connect-endpoints",
            Some(&json!({
                "Filters": [state_filter],
                "InstanceConnectEndpointIds": [endpoint_id],
            })),
        )?;
        return described
            .get("InstanceConnectEndpoints")
            .and_then(Value::as_array)
            .and_then(|endpoints| endpoints.first())
            .cloned()
            .ok_or_else(|| {
                param_error(&format!(
                    "There are no available instance connect endpoints with {endpoint_id}"
                ))
            });
    }

    // Paginated, and the walk stops at the first endpoint in the instance's own subnet —
    // a same-subnet endpoint avoids a cross-AZ hop for every byte of the tunnel.
    let mut first_in_vpc: Option<Value> = None;
    let mut token: Option<String> = None;
    loop {
        let mut input = json!({
            "Filters": [state_filter, { "Name": "vpc-id", "Values": [vpc_id.unwrap_or_default()] }],
        });
        if let Some(token) = &token {
            input["NextToken"] = Value::String(token.clone());
        }
        let described = ec2.call("describe-instance-connect-endpoints", Some(&input))?;
        if let Some(page) =
            described.get("InstanceConnectEndpoints").and_then(Value::as_array)
        {
            for endpoint in page {
                if endpoint.get("SubnetId").and_then(Value::as_str) == subnet_id {
                    return Ok(endpoint.clone());
                }
            }
            if first_in_vpc.is_none() {
                first_in_vpc = page.first().cloned();
            }
        }
        token = described.get("NextToken").and_then(Value::as_str).map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    first_in_vpc
        .ok_or_else(|| param_error("There are no available instance connect endpoints."))
}

/// `DnsName`, or `FipsDnsName` when FIPS endpoints are asked for.
fn eice_dns_name(endpoint: &Value, fips: bool) -> Result<String, Failure> {
    let name = |key: &str| endpoint.get(key).and_then(Value::as_str).map(str::to_string);
    if !fips {
        return name("DnsName")
            .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'DnsName'"));
    }
    // A ConfigurationError in the reference, which is 253 here too: the user asked for
    // FIPS and this endpoint cannot give it, so falling back silently would be worse.
    name("FipsDnsName")
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, "Unable to find FIPS Endpoint"))
}

/// `AWS_USE_FIPS_ENDPOINT`, then the profile's `use_fips_endpoint`.
///
/// Read here rather than taken from the endpoint resolver, which does not implement it:
/// the EICE DNS name is chosen from the endpoint record, not resolved from a ruleset.
fn use_fips_endpoint(globals: &Globals) -> bool {
    let truthy = |value: &str| matches!(value.to_ascii_lowercase().as_str(), "true" | "1");
    if let Ok(value) = std::env::var("AWS_USE_FIPS_ENDPOINT") {
        return truthy(&value);
    }
    awsc_runtime::credentials::profile::Config::load()
        .ok()
        .and_then(|config| {
            let name =
                awsc_runtime::credentials::profile::profile_name(globals.profile.as_deref());
            config.profile(&name).and_then(|section| section.get("use_fips_endpoint").cloned())
        })
        .map(|value| truthy(&value))
        .unwrap_or(false)
}

#[cfg(unix)]
fn stdin_is_a_terminal() -> bool {
    // SAFETY: `isatty` only inspects the descriptor.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdin_is_a_terminal() -> bool {
    // Without `isatty` the safe answer is "not a terminal": refusing to run would break
    // the proxy use, which is the one that matters.
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> awsc_runtime::credentials::Credentials {
        awsc_runtime::credentials::Credentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
            expires_at: None,
            method: "env",
        }
    }

    fn signer() -> Signer {
        Signer {
            credentials: credentials(),
            region: "us-east-1".to_string(),
            dns_name: "eice-0123.ec2-instance-connect-endpoint.us-east-1.amazonaws.com"
                .to_string(),
            endpoint_id: "eice-0123".to_string(),
            remote_port: 22,
            private_ip: "10.0.0.5".to_string(),
            max_tunnel_duration: None,
        }
    }

    /// The tunnel parameters come first and in the order the reference builds the dict,
    /// then the auth parameters. A server that recomputes the signature from a different
    /// order still agrees, but the *emitted* order is what a reader compares.
    #[test]
    fn the_presigned_url_carries_the_tunnel_parameters_first() {
        let url = signer().presigned_url();
        assert!(url.starts_with(
            "wss://eice-0123.ec2-instance-connect-endpoint.us-east-1.amazonaws.com/openTunnel?\
             instanceConnectEndpointId=eice-0123&remotePort=22&privateIpAddress=10.0.0.5&\
             X-Amz-Algorithm=AWS4-HMAC-SHA256&"
        ), "{url}");
        assert!(url.contains("X-Amz-Expires=60&"), "{url}");
        assert!(url.contains("X-Amz-SignedHeaders=host&"), "{url}");
        assert!(url.contains("&X-Amz-Signature="), "{url}");
    }

    /// `maxTunnelDuration` is only sent when asked for, and it sits with the tunnel
    /// parameters rather than after the auth ones.
    #[test]
    fn the_max_duration_joins_the_tunnel_parameters() {
        let mut signer = signer();
        signer.max_tunnel_duration = Some(120);
        let url = signer.presigned_url();
        assert!(
            url.contains("privateIpAddress=10.0.0.5&maxTunnelDuration=120&X-Amz-Algorithm="),
            "{url}"
        );
    }

    #[test]
    fn fips_picks_the_fips_name_and_refuses_when_there_is_none() {
        let with_fips = json!({ "DnsName": "plain", "FipsDnsName": "fips" });
        assert_eq!(eice_dns_name(&with_fips, false).expect("plain"), "plain");
        assert_eq!(eice_dns_name(&with_fips, true).expect("fips"), "fips");
        let without = json!({ "DnsName": "plain" });
        assert_eq!(eice_dns_name(&without, false).expect("plain"), "plain");
        let failure = eice_dns_name(&without, true).expect_err("refuses");
        assert_eq!(failure.message(), "Unable to find FIPS Endpoint");
    }

    /// 1000 is a clean close; anything else is reported with the server's own reason.
    #[test]
    fn only_a_non_normal_close_is_an_error() {
        assert!(closure_reason(&[0x03, 0xE8]).is_ok());
        assert!(closure_reason(&[]).is_ok());
        let mut payload = 1011u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"Endpoint unavailable");
        assert_eq!(
            closure_reason(&payload).expect_err("reports"),
            "Websocket Closure Reason: Endpoint unavailable"
        );
    }
}
