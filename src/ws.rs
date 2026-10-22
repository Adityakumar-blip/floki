use crate::model::{resolve, Request};
use futures::channel::mpsc::UnboundedSender;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{HeaderName, HeaderValue};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Connector, Message};

/// How long the worker blocks on a read before checking for outgoing messages.
const POLL: Duration = Duration::from_millis(50);

pub enum Command {
    Text(String),
    Binary(Vec<u8>),
    /// Socket.IO event; `data` is sent as JSON when it parses, else as a string.
    Emit { event: String, data: String, ack: bool },
    Close,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// Handshake response status line and headers.
    Connected(String, Vec<(String, String)>),
    /// `event` is the Socket.IO event name, or `ack <id>` for acknowledgements.
    Received { text: String, binary: bool, event: Option<String> },
    Reconnecting(String),
    Closed(String),
    Failed(String),
}

enum End {
    /// Finished; the final event was already emitted.
    Stop,
    /// Dropped unexpectedly; `connected` says whether the handshake had succeeded.
    Lost { reason: String, connected: bool },
}

/// Opens the WebSocket or Socket.IO connection described by `request` and pumps it until it closes
/// or `commands` is dropped, reconnecting per the request's settings. Blocking; run it on its own thread.
pub fn run(request: &Request, vars: &[(String, String)], commands: Receiver<Command>, events: UnboundedSender<Event>) {
    let emit = |event| {
        let _ = events.unbounded_send(event);
    };
    let settings = &request.socket;
    let mut attempts_left = settings.reconnect_attempts;
    let mut ever_connected = false;
    loop {
        match session(request, vars, &commands, &emit) {
            End::Stop => return,
            End::Lost { reason, connected } => {
                ever_connected |= connected;
                if connected {
                    attempts_left = settings.reconnect_attempts;
                }
                if !ever_connected || attempts_left == 0 {
                    return emit(Event::Failed(reason));
                }
                attempts_left -= 1;
                let attempt = settings.reconnect_attempts - attempts_left;
                emit(Event::Reconnecting(format!(
                    "{reason}. Reconnecting ({attempt}/{})",
                    settings.reconnect_attempts
                )));
                let deadline = Instant::now() + Duration::from_millis(settings.reconnect_interval_ms);
                while Instant::now() < deadline {
                    match commands.recv_timeout(POLL) {
                        Ok(Command::Close) => return emit(Event::Closed("Disconnected".into())),
                        Err(RecvTimeoutError::Disconnected) => return,
                        _ => {}
                    }
                }
            }
        }
    }
}

fn session(request: &Request, vars: &[(String, String)], commands: &Receiver<Command>, emit: &impl Fn(Event)) -> End {
    let (mut socket, tcp, status, headers, namespace) = match connect(request, vars) {
        Ok(parts) => parts,
        Err(reason) => return End::Lost { reason, connected: false },
    };
    let socket_io = request.is_socket_io();
    // Socket.IO is connected once the namespace accepts us, not at the WebSocket handshake.
    let mut pending = if socket_io {
        Some((status, headers))
    } else {
        emit(Event::Connected(status, headers));
        None
    };
    let ns = if namespace == "/" { String::new() } else { format!("{namespace},") };
    let mut next_ack = 0_u64;
    // Short reads let one thread both read and write; tungstenite resumes partial frames.
    let _ = tcp.set_read_timeout(Some(POLL));
    let mut closing: Option<Instant> = None;
    let mut close_reason = String::from("Disconnected");
    let mut abnormal = false;
    let lost = |reason: String, pending: &Option<_>| End::Lost {
        reason,
        connected: pending.is_none(),
    };
    loop {
        let outgoing = match commands.try_recv() {
            Ok(Command::Text(text)) => Some(Message::text(text)),
            Ok(Command::Binary(bytes)) => Some(Message::binary(bytes)),
            Ok(Command::Emit { event, data, ack }) => {
                let mut packet = vec![serde_json::Value::String(event)];
                if !data.is_empty() {
                    packet.push(serde_json::from_str(&data).unwrap_or(serde_json::Value::String(data)));
                }
                let id = if ack {
                    next_ack += 1;
                    next_ack.to_string()
                } else {
                    String::new()
                };
                Some(Message::text(format!("42{ns}{id}{}", serde_json::Value::Array(packet))))
            }
            Ok(Command::Close) if closing.is_none() => {
                if socket_io {
                    let _ = socket.send(Message::text(format!("41{ns}")));
                }
                let _ = socket.close(None);
                closing = Some(Instant::now());
                None
            }
            Ok(Command::Close) | Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // The tab went away; nobody is listening for the close handshake.
                let _ = socket.close(None);
                let _ = socket.flush();
                return End::Stop;
            }
        };
        if let Some(message) = outgoing {
            if let Err(e) = socket.send(message) {
                return lost(e.to_string(), &pending);
            }
        }
        if closing.is_some_and(|at| at.elapsed() > Duration::from_secs(5)) {
            emit(Event::Closed(close_reason));
            return End::Stop;
        }
        match socket.read() {
            Ok(Message::Text(text)) if socket_io => match text.as_bytes().first() {
                Some(b'0') => {
                    if let Err(e) = socket.send(Message::text(format!("40{ns}"))) {
                        return lost(e.to_string(), &pending);
                    }
                }
                Some(b'2') => {
                    let _ = socket.send(Message::text("3"));
                }
                Some(b'1') => return lost("Server closed the Engine.IO session".into(), &pending),
                Some(b'4') => match parse_socket_io(&text) {
                    Some(('0', _, _)) => {
                        if let Some((status, headers)) = pending.take() {
                            emit(Event::Connected(status, headers));
                        }
                    }
                    Some(('1', _, _)) => {
                        let _ = socket.close(None);
                        emit(Event::Closed("Disconnected by server".into()));
                        return End::Stop;
                    }
                    Some(('4', _, data)) => {
                        let message = serde_json::from_str::<serde_json::Value>(data)
                            .ok()
                            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                            .unwrap_or_else(|| data.to_string());
                        let _ = socket.close(None);
                        emit(Event::Failed(format!("Connection refused: {message}")));
                        return End::Stop;
                    }
                    Some((kind @ ('2' | '3'), id, data)) => {
                        let mut args = match serde_json::from_str::<serde_json::Value>(data) {
                            Ok(serde_json::Value::Array(args)) => args,
                            _ => vec![serde_json::Value::String(data.into())],
                        };
                        let event = if kind == '2' && !args.is_empty() {
                            match args.remove(0) {
                                serde_json::Value::String(name) => name,
                                other => other.to_string(),
                            }
                        } else {
                            format!("ack {}", id.unwrap_or_default())
                        };
                        let text = match args.as_slice() {
                            [] => String::new(),
                            [serde_json::Value::String(s)] => s.clone(),
                            [one] => one.to_string(),
                            many => serde_json::Value::Array(many.to_vec()).to_string(),
                        };
                        emit(Event::Received { text, binary: false, event: Some(event) })
                    }
                    // ponytail: binary events (5/6) arrive as raw packets; attachments are not reassembled.
                    _ => emit(Event::Received { text: text.to_string(), binary: false, event: None }),
                },
                _ => {} // Engine.IO pong/noop/upgrade packets
            },
            Ok(Message::Text(text)) => emit(Event::Received {
                text: text.to_string(),
                binary: false,
                event: None,
            }),
            Ok(Message::Binary(bytes)) => emit(Event::Received {
                text: String::from_utf8(bytes.to_vec()).unwrap_or_else(|e| hex(e.as_bytes())),
                binary: true,
                event: None,
            }),
            Ok(Message::Close(frame)) => {
                if let Some(frame) = frame {
                    let code = u16::from(frame.code);
                    abnormal = code != 1000 && code != 1001;
                    close_reason = match frame.reason.as_str() {
                        "" => format!("Disconnected ({code})"),
                        reason => format!("Disconnected ({code}: {reason})"),
                    };
                }
            }
            Ok(_) => {} // ping/pong are answered by tungstenite
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed)
                if closing.is_some() || !abnormal =>
            {
                emit(Event::Closed(close_reason));
                return End::Stop;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return lost(close_reason, &pending)
            }
            Err(e) => return lost(e.to_string(), &pending),
        }
    }
}

/// Splits a Socket.IO packet ("4" + type + optional "/ns," + optional ack id + JSON) into (type, ack id, JSON).
fn parse_socket_io(packet: &str) -> Option<(char, Option<u64>, &str)> {
    let rest = packet.strip_prefix('4')?;
    let kind = rest.chars().next()?;
    let mut rest = &rest[kind.len_utf8()..];
    if rest.starts_with('/') {
        rest = rest.split_once(',').map_or("", |(_, r)| r);
    }
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    Some((kind, rest[..digits].parse().ok(), &rest[digits..]))
}

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

/// Returns the socket, a handle to its TCP stream, the handshake status and headers, and the Socket.IO namespace.
fn connect(
    request: &Request,
    vars: &[(String, String)],
) -> Result<(Socket, TcpStream, String, Vec<(String, String)>, String), String> {
    let socket_io = request.is_socket_io();
    let mut url = resolve(request.url.trim(), vars);
    if url.is_empty() {
        return Err("Enter a URL to connect".into());
    }
    if !url.contains("://") {
        url = format!("{}://{url}", if socket_io { "http" } else { "ws" });
    }
    let mut parsed = url::Url::parse(&url).map_err(|e| format!("Invalid URL \"{url}\": {e}"))?;
    let scheme = match (parsed.scheme(), socket_io) {
        ("ws", _) | ("http", true) => "ws",
        ("wss", _) | ("https", true) => "wss",
        (other, _) => {
            let allowed = if socket_io { "http(s):// or ws(s)://" } else { "ws:// or wss://" };
            return Err(format!("Unsupported protocol \"{other}\", use {allowed}"));
        }
    };
    parsed.set_scheme(scheme).map_err(|_| "Invalid URL scheme")?;
    let mut namespace = String::from("/");
    if socket_io {
        namespace = match parsed.path().trim_end_matches('/') {
            "" => "/".into(),
            path => path.into(),
        };
        let handshake = resolve(request.socket.handshake_path.trim(), vars);
        let handshake = handshake.trim_matches('/');
        parsed.set_path(&format!("/{}/", if handshake.is_empty() { "socket.io" } else { handshake }));
        parsed.query_pairs_mut().append_pair("EIO", "4").append_pair("transport", "websocket");
    }
    let host = parsed.host_str().ok_or("URL has no host")?.trim_matches(['[', ']']).to_string();
    let port = parsed.port_or_known_default().ok_or("URL has no port")?;

    let mut handshake = parsed.as_str().into_client_request().map_err(|e| e.to_string())?;
    let headers = handshake.headers_mut();
    for h in request.headers.iter().filter(|h| h.enabled && !h.key.trim().is_empty()) {
        let key = resolve(h.key.trim(), vars);
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| format!("Invalid header name \"{key}\""))?;
        let value = HeaderValue::from_str(&resolve(&h.value, vars)).map_err(|_| format!("Invalid value for header \"{key}\""))?;
        headers.append(name, value);
    }
    if !headers.contains_key("user-agent") {
        headers.insert(
            "user-agent",
            HeaderValue::from_static(concat!("FlokiRuntime/", env!("CARGO_PKG_VERSION"))),
        );
    }

    let timeout = Some(Duration::from_millis(request.socket.handshake_timeout_ms)).filter(|t| !t.is_zero());
    let addrs = (host.as_str(), port).to_socket_addrs().map_err(|e| format!("Could not resolve {host}: {e}"))?;
    let mut last_error = format!("Could not resolve {host}");
    let tcp = addrs
        .into_iter()
        .find_map(|addr| {
            match timeout {
                Some(t) => TcpStream::connect_timeout(&addr, t),
                None => TcpStream::connect(addr),
            }
            .map_err(|e| last_error = format!("Could not connect to {addr}: {e}"))
            .ok()
        })
        .ok_or(last_error)?;
    tcp.set_read_timeout(timeout).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(timeout).map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let handle = tcp.try_clone().map_err(|e| e.to_string())?;
    let max = (request.socket.max_message_mb.max(1) as usize).saturating_mul(1024 * 1024);
    let config = WebSocketConfig::default().max_message_size(Some(max)).max_frame_size(Some(max));
    let connector = (!request.verify_ssl).then(insecure_connector);
    let (socket, response) = tungstenite::client_tls_with_config(handshake, tcp, Some(config), connector)
        .map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Io(io)) if io.kind() == ErrorKind::WouldBlock || io.kind() == ErrorKind::TimedOut => {
                "Handshake timed out".to_string()
            }
            e => e.to_string(),
        })?;
    let status = format!(
        "{} {}",
        response.status().as_u16(),
        response.status().canonical_reason().unwrap_or_default()
    );
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    Ok((socket, handle, status, headers, namespace))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// TLS that skips certificate checks, for the "SSL certificate verification" setting being off.
fn insecure_connector() -> Connector {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}

#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{KeyValue, SOCKET_IO};
    use futures::StreamExt;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    type Server = tungstenite::WebSocket<TcpStream>;

    fn listen() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn start(request: Request, vars: Vec<(String, String)>) -> (mpsc::Sender<Command>, futures::channel::mpsc::UnboundedReceiver<Event>, thread::JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
        let worker = thread::spawn(move || run(&request, &vars, cmd_rx, ev_tx));
        (cmd_tx, ev_rx, worker)
    }

    fn next(rx: &mut futures::channel::mpsc::UnboundedReceiver<Event>) -> Event {
        futures::executor::block_on(rx.next()).unwrap()
    }

    fn received(text: &str, binary: bool, event: Option<&str>) -> Event {
        Event::Received { text: text.into(), binary, event: event.map(String::from) }
    }

    /// Reads frames until the client closes.
    fn drain(ws: &mut Server, mut on_text: impl FnMut(&mut Server, String)) {
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => on_text(ws, t.to_string()),
                Ok(Message::Binary(b)) => ws.send(Message::text(format!("got {} bytes", b.len()))).unwrap(),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    #[test]
    fn connects_with_headers_echoes_and_closes() {
        let (listener, port) = listen();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut seen = None;
            let mut ws = tungstenite::accept_hdr(stream, |req: &tungstenite::handshake::server::Request, res| {
                seen = req.headers().get("x-token").map(|v| v.to_str().unwrap().to_string());
                Ok(res)
            })
            .unwrap();
            ws.send(Message::text(seen.unwrap_or_default())).unwrap();
            drain(&mut ws, |ws, t| ws.send(Message::Binary(format!("echo {t}").into_bytes().into())).unwrap());
        });

        let mut request = Request::default();
        request.set_url(format!("ws://127.0.0.1:{port}/socket"));
        assert_eq!(request.method, "WS");
        request.headers.push(KeyValue::new("X-Token", "{{t}}"));
        let (tx, mut rx, worker) = start(request, vec![("t".into(), "abc".into())]);

        assert!(matches!(next(&mut rx), Event::Connected(status, _) if status.starts_with("101")));
        assert_eq!(next(&mut rx), received("abc", false, None));
        tx.send(Command::Text("hi".into())).unwrap();
        assert_eq!(next(&mut rx), received("echo hi", true, None));
        tx.send(Command::Binary(vec![0, 159, 146])).unwrap();
        assert_eq!(next(&mut rx), received("got 3 bytes", false, None));
        tx.send(Command::Close).unwrap();
        assert!(matches!(next(&mut rx), Event::Closed(_)));
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn reconnects_after_a_dropped_connection() {
        let (listener, port) = listen();
        let server = thread::spawn(move || {
            // First connection dies without a close frame; the second stays until the client closes.
            let (stream, _) = listener.accept().unwrap();
            drop(tungstenite::accept(stream).unwrap());
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            drain(&mut ws, |_, _| {});
        });
        let mut request = Request::default();
        request.set_url(format!("ws://127.0.0.1:{port}"));
        request.socket.reconnect_attempts = 2;
        request.socket.reconnect_interval_ms = 10;
        let (tx, mut rx, worker) = start(request, vec![]);
        assert!(matches!(next(&mut rx), Event::Connected(..)));
        assert!(matches!(next(&mut rx), Event::Reconnecting(m) if m.contains("(1/2)")));
        assert!(matches!(next(&mut rx), Event::Connected(..)));
        tx.send(Command::Close).unwrap();
        assert!(matches!(next(&mut rx), Event::Closed(_)));
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn speaks_socket_io_v4() {
        let (listener, port) = listen();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut path = String::new();
            let mut ws = tungstenite::accept_hdr(stream, |req: &tungstenite::handshake::server::Request, res| {
                path = req.uri().to_string();
                Ok(res)
            })
            .unwrap();
            assert!(path.starts_with("/io/?room=1&EIO=4&transport=websocket"), "{path}");
            ws.send(Message::text(r#"0{"sid":"e1","pingInterval":25000,"pingTimeout":20000}"#)).unwrap();
            ws.send(Message::text("2")).unwrap();
            let mut pong = false;
            drain(&mut ws, |ws, packet| match packet.as_str() {
                "3" => pong = true,
                "40/chat," => {
                    ws.send(Message::text(r#"40/chat,{"sid":"s1"}"#)).unwrap();
                    ws.send(Message::text(r#"42/chat,["news",{"a":1}]"#)).unwrap();
                }
                r#"42/chat,1["hello",{"x":true}]"# => ws.send(Message::text(r#"43/chat,1["ok"]"#)).unwrap(),
                r#"42/chat,["plain","text"]"# => ws.send(Message::text(r#"42/chat,["plain","text"]"#)).unwrap(),
                "41/chat," => {}
                other => panic!("unexpected packet {other}"),
            });
            assert!(pong);
        });
        let mut request = Request::default();
        request.method = SOCKET_IO.into();
        request.set_url(format!("http://127.0.0.1:{port}/chat?room=1"));
        assert!(request.is_socket_io(), "http URLs keep Socket.IO");
        request.socket.handshake_path = "io".into();
        let (tx, mut rx, worker) = start(request, vec![]);
        assert!(matches!(next(&mut rx), Event::Connected(..)));
        assert_eq!(next(&mut rx), received(r#"{"a":1}"#, false, Some("news")));
        tx.send(Command::Emit { event: "hello".into(), data: r#"{"x": true}"#.into(), ack: true }).unwrap();
        assert_eq!(next(&mut rx), received("ok", false, Some("ack 1")));
        tx.send(Command::Emit { event: "plain".into(), data: "text".into(), ack: false }).unwrap();
        assert_eq!(next(&mut rx), received("text", false, Some("plain")));
        tx.send(Command::Close).unwrap();
        assert!(matches!(next(&mut rx), Event::Closed(_)));
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn parses_socket_io_packets() {
        assert_eq!(parse_socket_io(r#"42["a"]"#), Some(('2', None, r#"["a"]"#)));
        assert_eq!(parse_socket_io(r#"43/admin,12["ok"]"#), Some(('3', Some(12), r#"["ok"]"#)));
        assert_eq!(parse_socket_io("41"), Some(('1', None, "")));
        assert_eq!(parse_socket_io("2"), None);
    }

    #[test]
    fn reports_bad_urls_and_refused_connections() {
        let mut request = Request::default();
        request.url = "http://example.com".into();
        let (_tx, mut rx, worker) = start(request, vec![]);
        assert!(matches!(next(&mut rx), Event::Failed(e) if e.contains("Unsupported protocol")));
        worker.join().unwrap();

        let (listener, port) = listen();
        drop(listener);
        let mut request = Request::default();
        request.url = format!("ws://127.0.0.1:{port}");
        request.socket.reconnect_attempts = 3;
        let (_tx, mut rx, worker) = start(request, vec![]);
        assert!(matches!(next(&mut rx), Event::Failed(e) if e.contains("Could not connect")), "no retries before the first connection");
        worker.join().unwrap();
    }
}
