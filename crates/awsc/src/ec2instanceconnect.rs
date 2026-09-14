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
        "ssh" => ssh(parsed, globals).map(Some),
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

/// `aws ec2-instance-connect ssh`: push a throwaway key, then hand over to OpenSSH.
///
/// Two things to know before reading the connection logic below:
///
/// - **A fresh Ed25519 key pair is generated per invocation** unless `--private-key-file`
///   says otherwise. The public half goes to the instance through `send-ssh-public-key`,
///   where it is valid for 60 seconds; the private half is written to a file this command
///   creates `0400` and deletes on the way out.
/// - **`--connection-type auto` decides between a direct connection and a tunnel by which
///   addresses the instance has** — public IPv4 means direct, a private IPv4 alone means
///   a tunnel, IPv6 alone means direct. The reference's own comment says this may change,
///   which is a reason to pass `--connection-type` explicitly rather than a reason to
///   simplify it here.
fn ssh(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(
        parsed,
        &[
            "--instance-id",
            "--instance-ip",
            "--private-key-file",
            "--os-user",
            "--ssh-port",
            "--local-forwarding",
            "--connection-type",
            "--eice-options",
        ],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();

    let Some(instance_id) = value("--instance-id") else {
        return Err(crate::custom::missing_required(&["--instance-id"]));
    };
    let os_user = value("--os-user").unwrap_or("ec2-user");
    let ssh_port = value("--ssh-port").unwrap_or("22");
    let connection_type = value("--connection-type").unwrap_or("auto");
    let eice_options = match value("--eice-options") {
        Some(token) => Some(crate::custom::parse_shorthand_token(token, "--eice-options")?),
        None => None,
    };
    let option = |name: &str| -> Option<String> {
        eice_options.as_ref().and_then(|options| options.get(name)).and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    };

    validate_ssh_args(
        instance_id,
        value("--instance-ip"),
        connection_type,
        eice_options.as_ref(),
        &option,
    )?;

    let region = resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    // Unlike `open-tunnel`, this one *does* forward `--endpoint-url` to EC2: the
    // reference passes it here and omits it there. Surprising, but it is the behaviour.
    // Below the flag, botocore still applies `AWS_ENDPOINT_URL_EC2`, so that is the
    // fallback rather than "no override at all".
    let ec2_globals = Globals {
        region: Some(region.clone()),
        endpoint_url: globals
            .endpoint_url
            .clone()
            .or_else(|| Globals::endpoint_from_environment("ec2")),
        ..globals.clone()
    };
    let ec2_model = crate::load_model("ec2").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let ec2 = Client::new(&ec2_model, &ec2_globals)?;
    let described =
        ec2.call("describe-instances", Some(&json!({ "InstanceIds": [instance_id] })))?;
    let instance = described
        .get("Reservations")
        .and_then(|r| r.get(0))
        .and_then(|r| r.get("Instances"))
        .and_then(|i| i.get(0))
        .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "list index out of range"))?;
    let address = |key: &str| instance.get(key).and_then(Value::as_str).map(str::to_string);

    let (use_tunnel, ip_address) = choose_connection(
        value("--instance-ip"),
        connection_type,
        eice_options.is_some(),
        address("PublicIpAddress"),
        address("PrivateIpAddress"),
        address("Ipv6Address"),
    );
    let Some(ip_address) = ip_address else {
        return Err(param_error("Unable to find any IP address on the instance to connect to."));
    };

    let mut endpoint_id = option("endpointId");
    let mut dns_name = option("dnsName");
    if use_tunnel && dns_name.is_none() {
        let endpoint = find_endpoint(
            &ec2,
            instance.get("VpcId").and_then(Value::as_str),
            instance.get("SubnetId").and_then(Value::as_str),
            endpoint_id.as_deref(),
        )?;
        endpoint_id = endpoint
            .get("InstanceConnectEndpointId")
            .and_then(Value::as_str)
            .map(str::to_string);
        dns_name = Some(eice_dns_name(&endpoint, use_fips_endpoint(globals))?);
    }

    // The generated key lives in its own directory so the file can be removed with it,
    // and so a key never lands in a directory the user did not expect.
    let mut generated: Option<std::path::PathBuf> = None;
    let key_file = match value("--private-key-file") {
        Some(path) => path.to_string(),
        None => {
            let key = crate::sshkey::generate()
                .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
            let eic_globals =
                Globals { region: Some(region.clone()), ..globals.for_service("ec2-instance-connect") };
            let eic_model = crate::load_model("ec2-instance-connect")
                .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
            let eic = Client::new(&eic_model, &eic_globals)?;
            eic.call(
                "send-ssh-public-key",
                Some(&json!({
                    "InstanceId": instance_id,
                    "InstanceOSUser": os_user,
                    "SSHPublicKey": crate::sshkey::authorized_key(&key),
                })),
            )?;
            let path = write_private_key(&key)?;
            generated = Some(path.clone());
            path.to_string_lossy().into_owned()
        }
    };

    let outcome = run_ssh(
        RunSsh {
            use_tunnel,
            instance_id,
            ssh_port,
            os_user,
            local_forwarding: value("--local-forwarding"),
            key_file: &key_file,
            ip_address: &ip_address,
            endpoint_id: endpoint_id.as_deref(),
            dns_name: dns_name.as_deref(),
            max_tunnel_duration: option("maxTunnelDuration"),
        },
        globals,
    );
    // Removed whether ssh succeeded or not: the key is good for one login and leaving it
    // on disk is the only way this command can leave a credential behind.
    if let Some(path) = generated {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap_or(&path));
    }
    outcome
}

/// The reference's validation, in its order and with its wording.
fn validate_ssh_args(
    instance_id: &str,
    instance_ip: Option<&str>,
    connection_type: &str,
    eice_options: Option<&Value>,
    option: &dyn Fn(&str) -> Option<String>,
) -> Result<(), Failure> {
    if !is_instance_id(instance_id) {
        return Err(param_error(
            "The specified instance ID is invalid. Provide the full instance ID in the form \
             i-xxxxxxxxxxxxxxxxx.",
        ));
    }
    if connection_type == "direct" && eice_options.is_some() {
        return Err(param_error(
            "eice-options can't be specified when connection type is direct.",
        ));
    }
    if option("dnsName").is_some() && option("endpointId").is_none() {
        return Err(param_error("When specifying dnsName, you must specify endpointId."));
    }
    if let Some(duration) = option("maxTunnelDuration") {
        let duration: u32 = duration
            .parse()
            .map_err(|_| param_error("Invalid value specified for maxTunnelDuration."))?;
        if !(1..=3_600).contains(&duration) {
            return Err(param_error(
                "Invalid value specified for maxTunnelDuration. Value must be greater than 1 \
                 and less than 3600.",
            ));
        }
    }
    if let Some(endpoint_id) = option("endpointId") {
        if !matches_pattern(&endpoint_id, "eice-", |c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(param_error(
                "The specified endpointId is invalid. Provide the full EC2 Instance Connect \
                 Endpoint ID in the form eice-xxxxxxxxxxxxxxxxx.",
            ));
        }
    }
    if let Some(dns_name) = option("dnsName") {
        if dns_name.is_empty()
            || !dns_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(param_error("The specified dnsName is invalid."));
        }
    }
    // `auto` is the default, so this fires whenever `--instance-ip` is given without an
    // explicit `--connection-type`: with an address supplied by hand, the command will
    // not guess whether it is reachable directly.
    if instance_ip.is_some() && connection_type == "auto" {
        return Err(param_error(
            "When specifying instance-ip, you must specify connection-type.",
        ));
    }
    Ok(())
}

fn is_instance_id(value: &str) -> bool {
    matches_pattern(value, "i-", |c| c.is_ascii_alphanumeric())
}

/// `^<prefix>[...]+$`: the prefix, then at least one character the predicate accepts.
fn matches_pattern(value: &str, prefix: &str, allowed: impl Fn(char) -> bool) -> bool {
    match value.strip_prefix(prefix) {
        Some(rest) => !rest.is_empty() && rest.chars().all(allowed),
        None => false,
    }
}

/// Which address to connect to, and whether it needs a tunnel.
///
/// Split out because it is the whole of the command's behaviour that is worth testing
/// without an instance: six branches, and the `auto` one has a documented preference
/// order that a reader would otherwise have to reconstruct from the API calls.
fn choose_connection(
    instance_ip: Option<&str>,
    connection_type: &str,
    has_eice_options: bool,
    public_ipv4: Option<String>,
    private_ipv4: Option<String>,
    ipv6: Option<String>,
) -> (bool, Option<String>) {
    if let Some(instance_ip) = instance_ip {
        return (connection_type == "eice", Some(instance_ip.to_string()));
    }
    // `--eice-options` on its own asks for a tunnel, without `--connection-type eice`.
    if connection_type == "eice" || has_eice_options {
        return (true, private_ipv4.or(ipv6));
    }
    if connection_type == "direct" {
        return (false, public_ipv4.or(ipv6).or(private_ipv4));
    }
    // auto: IPv4 before IPv6, because that is what most instances have today.
    match (public_ipv4, private_ipv4, ipv6) {
        (Some(public), _, _) => (false, Some(public)),
        (None, Some(private), _) => (true, Some(private)),
        (None, None, Some(ipv6)) => (false, Some(ipv6)),
        (None, None, None) => (false, None),
    }
}

/// Write the private key where only this user can read it.
fn write_private_key(key: &crate::sshkey::Ed25519Key) -> Result<std::path::PathBuf, Failure> {
    let directory = std::env::temp_dir().join(format!(
        "awsc-eic-{}-{}",
        std::process::id(),
        crate::now_unix()
    ));
    std::fs::create_dir_all(&directory).map_err(|e| {
        Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", directory.display()))
    })?;
    let path = directory.join("private-key");
    std::fs::write(&path, crate::sshkey::private_pem(key))
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", path.display())))?;
    // `ssh` refuses a key file other users can read, so this is not merely hygiene.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", path.display())))?;
    }
    Ok(path)
}

struct RunSsh<'a> {
    use_tunnel: bool,
    instance_id: &'a str,
    ssh_port: &'a str,
    os_user: &'a str,
    local_forwarding: Option<&'a str>,
    key_file: &'a str,
    ip_address: &'a str,
    endpoint_id: Option<&'a str>,
    dns_name: Option<&'a str>,
    max_tunnel_duration: Option<String>,
}

fn run_ssh(options: RunSsh<'_>, globals: &Globals) -> Result<ExitCode, Failure> {
    // argv[0], as the reference uses `sys.argv[0]`: the ProxyCommand has to re-invoke
    // the binary the user actually ran, not whatever `aws` is on PATH.
    let argv0 = std::env::args().next().unwrap_or_else(|| "awsc".to_string());
    let command = ssh_command(&options, globals, &argv0);
    let status = std::process::Command::new(&command[0]).args(&command[1..]).status();
    match status {
        Ok(status) => Ok(exit::code(status.code().unwrap_or(1) as u8)),
        // A ConfigurationError in the reference, so 253 — and the message points at the
        // documentation rather than at a missing binary, because "install OpenSSH" is
        // what the reader has to do.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Failure::new(
            exit::CONFIGURATION,
            "SSH not available. Please refer to the documentation at \
             https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/Connect-using-EC2-Instance-Connect-Endpoint.html.",
        )),
        Err(e) => Err(Failure::new(exit::GENERAL_ERROR, e)),
    }
}

/// The `ssh` argument list. `argv0` is the path this binary was invoked as, which becomes
/// the `ProxyCommand`'s first word — so a tunnel is opened by *this* build, not by
/// whatever `aws` happens to be on PATH.
fn ssh_command(options: &RunSsh<'_>, globals: &Globals, argv0: &str) -> Vec<String> {
    let mut command: Vec<String> = vec![
        "ssh".to_string(),
        // Not configurable, and deliberately so in the reference: without it a tunnel
        // that dies leaves the client sitting on a dead session with no indication.
        "-o".to_string(),
        "ServerAliveInterval=5".to_string(),
        "-p".to_string(),
        options.ssh_port.to_string(),
        "-i".to_string(),
        options.key_file.to_string(),
    ];
    if let Some(forwarding) = options.local_forwarding {
        command.push("-L".to_string());
        command.push(forwarding.to_string());
    }
    if globals.debug {
        command.push("-v".to_string());
    }
    if options.use_tunnel {
        let mut proxy = vec![
            argv0.to_string(),
            "ec2-instance-connect".to_string(),
            "open-tunnel".to_string(),
            "--instance-id".to_string(),
            options.instance_id.to_string(),
            "--private-ip-address".to_string(),
            options.ip_address.to_string(),
            "--remote-port".to_string(),
            options.ssh_port.to_string(),
        ];
        if let Some(region) = &globals.region {
            proxy.push("--region".to_string());
            proxy.push(region.clone());
        }
        if let Some(profile) = &globals.profile {
            proxy.push("--profile".to_string());
            proxy.push(profile.clone());
        }
        if let Some(endpoint_id) = options.endpoint_id {
            proxy.push("--instance-connect-endpoint-id".to_string());
            proxy.push(endpoint_id.to_string());
        }
        if let Some(dns_name) = options.dns_name {
            proxy.push("--instance-connect-endpoint-dns-name".to_string());
            proxy.push(dns_name.to_string());
        }
        if let Some(duration) = &options.max_tunnel_duration {
            proxy.push("--max-tunnel-duration".to_string());
            proxy.push(duration.clone());
        }
        let quoted: Vec<String> = proxy.iter().map(|word| shell_quote(word)).collect();
        command.push("-o".to_string());
        command.push(format!("ProxyCommand={}", quoted.join(" ")));
    }
    command.push(format!("{}@{}", options.os_user, options.ip_address));
    command
}

/// botocore's `compat_shell_quote` on POSIX, which is `shlex.quote`: leave a word alone
/// when every character is safe, otherwise wrap it in single quotes and escape any
/// single quote as `'"'"'`.
fn shell_quote(word: &str) -> String {
    if word.is_empty() {
        return "''".to_string();
    }
    let safe = |c: char| {
        c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)
    };
    if word.chars().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod ssh_tests {
    use super::*;

    fn globals() -> Globals {
        Globals {
            region: None,
            profile: None,
            endpoint_url: None,
            debug: false,
            no_sign_request: false,
            verify_ssl: true,
            ca_bundle: None,
            read_timeout: None,
            connect_timeout: None,
        }
    }

    fn options<'a>(use_tunnel: bool) -> RunSsh<'a> {
        RunSsh {
            use_tunnel,
            instance_id: "i-abc",
            ssh_port: "22",
            os_user: "ec2-user",
            local_forwarding: None,
            key_file: "/tmp/k",
            ip_address: "10.0.0.5",
            endpoint_id: None,
            dns_name: None,
            max_tunnel_duration: None,
        }
    }

    /// The plain form: no tunnel, and `user@ip` stays last.
    #[test]
    fn a_direct_connection_is_a_bare_ssh_command() {
        let command = ssh_command(&options(false), &globals(), "aws");
        assert_eq!(
            command,
            vec![
                "ssh",
                "-o",
                "ServerAliveInterval=5",
                "-p",
                "22",
                "-i",
                "/tmp/k",
                "ec2-user@10.0.0.5",
            ]
        );
    }

    /// Every optional flag lands *before* `user@ip`, which OpenSSH requires: anything
    /// after the destination is taken as a remote command.
    #[test]
    fn the_destination_stays_last_whatever_is_added() {
        let mut globals = globals();
        globals.debug = true;
        globals.region = Some("us-east-1".to_string());
        let mut options = options(true);
        options.local_forwarding = Some("3336:remote.host:3306");
        options.endpoint_id = Some("eice-1");
        let command = ssh_command(&options, &globals, "/usr/local/bin/awsc");
        assert_eq!(command.last().expect("has a destination"), "ec2-user@10.0.0.5");
        assert_eq!(command[command.len() - 3], "-o");
        assert!(command.contains(&"-v".to_string()));
        assert_eq!(command[7], "-L");
        assert_eq!(command[8], "3336:remote.host:3306");
    }

    /// The ProxyCommand re-invokes *this* binary, carries the region and profile through,
    /// and is one shell-quoted word list.
    #[test]
    fn the_proxy_command_reinvokes_this_binary() {
        let mut globals = globals();
        globals.region = Some("us-east-1".to_string());
        globals.profile = Some("my profile".to_string());
        let mut options = options(true);
        options.endpoint_id = Some("eice-1");
        options.max_tunnel_duration = Some("120".to_string());
        let command = ssh_command(&options, &globals, "/opt/my tools/awsc");
        let proxy = command
            .iter()
            .find(|word| word.starts_with("ProxyCommand="))
            .expect("has a ProxyCommand");
        assert_eq!(
            proxy,
            "ProxyCommand='/opt/my tools/awsc' ec2-instance-connect open-tunnel \
             --instance-id i-abc --private-ip-address 10.0.0.5 --remote-port 22 \
             --region us-east-1 --profile 'my profile' \
             --instance-connect-endpoint-id eice-1 --max-tunnel-duration 120"
        );
    }

    /// A word with nothing unsafe in it is left alone; anything else is single-quoted,
    /// and a single quote inside is closed, escaped and reopened.
    #[test]
    fn shell_quoting_follows_shlex() {
        assert_eq!(shell_quote("plain-word_1.2/3"), "plain-word_1.2/3");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
    }

    /// The `auto` preference order, which is the part a reader cannot guess.
    #[test]
    fn auto_prefers_public_ipv4_then_private_then_ipv6() {
        let public = Some("1.2.3.4".to_string());
        let private = Some("10.0.0.5".to_string());
        let ipv6 = Some("2001:db8::1".to_string());
        // A public address is reachable directly.
        assert_eq!(
            choose_connection(None, "auto", false, public.clone(), private.clone(), ipv6.clone()),
            (false, public.clone())
        );
        // Private only: a tunnel.
        assert_eq!(
            choose_connection(None, "auto", false, None, private.clone(), ipv6.clone()),
            (true, private.clone())
        );
        // IPv6 only: direct, not a tunnel.
        assert_eq!(
            choose_connection(None, "auto", false, None, None, ipv6.clone()),
            (false, ipv6.clone())
        );
        assert_eq!(choose_connection(None, "auto", false, None, None, None), (false, None));
    }

    /// `direct` and `eice` have their own, different, preference orders — and
    /// `--eice-options` alone is enough to ask for a tunnel.
    #[test]
    fn the_explicit_connection_types_have_their_own_orders() {
        let public = Some("1.2.3.4".to_string());
        let private = Some("10.0.0.5".to_string());
        let ipv6 = Some("2001:db8::1".to_string());
        // direct: public, then IPv6, then private — IPv6 comes *before* the private IPv4.
        assert_eq!(
            choose_connection(None, "direct", false, None, private.clone(), ipv6.clone()),
            (false, ipv6.clone())
        );
        // eice: private IPv4, falling back to IPv6, and never the public address.
        assert_eq!(
            choose_connection(None, "eice", false, public.clone(), private.clone(), ipv6.clone()),
            (true, private.clone())
        );
        assert_eq!(
            choose_connection(None, "eice", false, public.clone(), None, ipv6.clone()),
            (true, ipv6.clone())
        );
        // eice-options with no --connection-type still means a tunnel.
        assert_eq!(
            choose_connection(None, "auto", true, public.clone(), private.clone(), None),
            (true, private.clone())
        );
        // An address given by hand is used as given; only the type decides the tunnel.
        assert_eq!(
            choose_connection(Some("192.0.2.7"), "eice", false, public, private, ipv6),
            (true, Some("192.0.2.7".to_string()))
        );
    }

    #[test]
    fn an_instance_id_must_look_like_one() {
        assert!(is_instance_id("i-0123456789abcdef0"));
        assert!(!is_instance_id("i-"));
        assert!(!is_instance_id("0123456789abcdef0"));
        assert!(!is_instance_id("i-abc_def"));
    }
}
