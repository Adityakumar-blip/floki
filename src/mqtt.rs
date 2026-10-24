//! MQTT 3.1.1 client for the socket message log. Driven by `ws::run`, which handles reconnects.

use crate::model::{resolve, Auth, Request};
use crate::ws::{handshake_timeout, hex, tcp_connect, tls_config, Command, End, Event};
use rustls::pki_types::ServerName;
use std::io::{ErrorKind, Read, Write};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

const CONNECT: u8 = 0x10;
const PUBLISH: u8 = 0x30;
const PUBACK: u8 = 0x40;
const PUBREC: u8 = 0x50;
const PUBREL: u8 = 0x62;
const PUBCOMP: u8 = 0x70;
const SUBSCRIBE: u8 = 0x82;
const PINGREQ: u8 = 0xC0;
const DISCONNECT: u8 = 0xE0;

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

struct Conn {
    stream: Box<dyn Stream>,
    buf: Vec<u8>,
    max: usize,
}

impl Conn {
    fn send(&mut self, header: u8, body: &[u8]) -> Result<(), String> {
        self.stream.write_all(&packet(header, body)).and_then(|_| self.stream.flush()).map_err(|e| e.to_string())
    }

    /// The next whole packet as (first byte, body), or `None` when nothing arrived within the read timeout.
    fn read(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        loop {
            if let Some((len, start)) = remaining_length(&self.buf)? {
                if len > self.max {
                    return Err(format!("Message of {len} bytes is over the size limit"));
                }
                if self.buf.len() >= start + len {
                    let header = self.buf[0];
                    let body = self.buf[start..start + len].to_vec();
                    self.buf.drain(..start + len);
                    return Ok(Some((header, body)));
                }
            }
            let mut chunk = [0_u8; 16 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("Broker closed the connection".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// Fixed header byte, variable-length remaining length, then `body`.
fn packet(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![header];
    let mut n = body.len();
    loop {
        let byte = (n % 128) as u8;
        n /= 128;
        out.push(if n > 0 { byte | 0x80 } else { byte });
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

/// (remaining length, offset of the body) once the fixed header is complete.
fn remaining_length(buf: &[u8]) -> Result<Option<(usize, usize)>, String> {
    let mut len = 0_usize;
    for i in 0..4 {
        let Some(&byte) = buf.get(1 + i) else { return Ok(None) };
        len += ((byte & 0x7F) as usize) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(Some((len, 2 + i)));
        }
    }
    Err("Malformed packet from broker".into())
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

fn take_u16(body: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]))
}

/// Splits a PUBLISH body into (topic, packet id, payload).
fn parse_publish(header: u8, body: &[u8]) -> Option<(String, Option<u16>, &[u8])> {
    let len = take_u16(body, 0)? as usize;
    let topic = String::from_utf8_lossy(body.get(2..2 + len)?).into_owned();
    let mut at = 2 + len;
    let id = if (header >> 1) & 3 > 0 {
        at += 2;
        Some(take_u16(body, at - 2)?)
    } else {
        None
    };
    Some((topic, id, body.get(at..)?))
}

fn connect_body(client_id: &str, auth: Option<(&str, &str)>, keep_alive: u16) -> Vec<u8> {
    let mut body = Vec::new();
    put_str(&mut body, b"MQTT");
    body.push(4); // protocol level 3.1.1
    let mut flags = 0x02; // clean session
    if let Some((_, pass)) = auth {
        flags |= 0x80;
        if !pass.is_empty() {
            flags |= 0x40;
        }
    }
    body.push(flags);
    body.extend_from_slice(&keep_alive.to_be_bytes());
    put_str(&mut body, client_id.as_bytes());
    if let Some((user, pass)) = auth {
        put_str(&mut body, user.as_bytes());
        if !pass.is_empty() {
            put_str(&mut body, pass.as_bytes());
        }
    }
    body
}

fn connack_error(code: u8) -> String {
    let reason = match code {
        1 => "unsupported protocol version",
        2 => "client ID rejected",
        3 => "broker unavailable",
        4 => "bad username or password",
        5 => "not authorized",
        _ => "unknown reason",
    };
    format!("Broker refused the connection: {reason} ({code})")
}

fn open(request: &Request, vars: &[(String, String)]) -> Result<(Conn, Vec<(String, String)>), String> {
    let mut url = resolve(request.url.trim(), vars);
    if url.is_empty() {
        return Err("Enter a broker URL to connect".into());
    }
    if !url.contains("://") {
        url = format!("mqtt://{url}");
    }
    let parsed = url::Url::parse(&url).map_err(|e| format!("Invalid URL \"{url}\": {e}"))?;
    let tls = match parsed.scheme() {
        "mqtt" | "tcp" => false,
        "mqtts" | "ssl" => true,
        other => return Err(format!("Unsupported protocol \"{other}\", use mqtt:// or mqtts://")),
    };
    let host = parsed.host_str().ok_or("URL has no host")?.trim_matches(['[', ']']).to_string();
    let port = parsed.port().unwrap_or(if tls { 8883 } else { 1883 });
    let auth = match &request.auth {
        Auth::None => None,
        Auth::Basic { username, password } => Some((resolve(username, vars), resolve(password, vars))),
        _ => return Err("MQTT supports Basic auth (username and password) only".into()),
    };

    let timeout = handshake_timeout(request);
    let tcp = tcp_connect(&host, port, timeout)?;
    let reader = tcp.try_clone().map_err(|e| e.to_string())?;
    let stream: Box<dyn Stream> = if tls {
        let name = ServerName::try_from(host.clone()).map_err(|_| format!("Invalid host name \"{host}\""))?;
        let conn = rustls::ClientConnection::new(tls_config(request.verify_ssl), name).map_err(|e| e.to_string())?;
        Box::new(rustls::StreamOwned::new(conn, tcp))
    } else {
        Box::new(tcp)
    };
    let max = (request.socket.max_message_mb.max(1) as usize).saturating_mul(1024 * 1024);
    let mut conn = Conn { stream, buf: Vec::new(), max };

    let client_id = match resolve(request.socket.client_id.trim(), vars) {
        id if id.is_empty() => {
            let mut bytes = [0_u8; 6];
            let _ = getrandom::fill(&mut bytes);
            format!("floki-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
        }
        id => id,
    };
    let auth = auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
    conn.send(CONNECT, &connect_body(&client_id, auth, request.socket.keep_alive_s))?;
    // Without a handshake timeout, wait up to 30s rather than hang on a silent server.
    let deadline = Instant::now() + timeout.unwrap_or(Duration::from_secs(30));
    let _ = reader.set_read_timeout(Some(POLL));
    let body = loop {
        match conn.read() {
            Ok(Some((header, body))) if header >> 4 == 2 => break body,
            Ok(Some(_)) => return Err("Broker sent an unexpected packet before CONNACK".into()),
            Ok(None) if Instant::now() < deadline => {}
            Ok(None) => return Err("Handshake timed out".into()),
            Err(e) => return Err(if tls { format!("TLS or connection error: {e}") } else { e }),
        }
    };
    match body.get(1) {
        Some(0) => {}
        Some(&code) => return Err(connack_error(code)),
        None => return Err("Malformed CONNACK".into()),
    }
    let session_present = body.first().is_some_and(|b| b & 1 == 1);
    let details = vec![
        ("Client ID".to_string(), client_id),
        ("Session present".to_string(), session_present.to_string()),
    ];
    Ok((conn, details))
}

pub(crate) fn session(request: &Request, vars: &[(String, String)], commands: &Receiver<Command>, emit: &impl Fn(Event)) -> End {
    let (mut conn, details) = match open(request, vars) {
        Ok(parts) => parts,
        Err(reason) => return End::Lost { reason, connected: false },
    };
    emit(Event::Connected("CONNACK accepted".into(), details));
    let lost = |reason: String| End::Lost { reason, connected: true };
    let qos = request.socket.qos.min(2);
    let mut next_id: u16 = 0;
    let mut id = || {
        next_id = next_id.checked_add(1).unwrap_or(1);
        next_id
    };

    let topics: Vec<String> = resolve(&request.socket.subscribe, vars)
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if !topics.is_empty() {
        let mut body = id().to_be_bytes().to_vec();
        for topic in &topics {
            put_str(&mut body, topic.as_bytes());
            body.push(qos);
        }
        if let Err(e) = conn.send(SUBSCRIBE, &body) {
            return lost(e);
        }
    }

    let keep_alive = Duration::from_secs(request.socket.keep_alive_s.into());
    let mut last_sent = Instant::now();
    let mut last_heard = Instant::now();
    loop {
        match commands.try_recv() {
            Ok(Command::Publish { topic, payload, qos, retain }) => {
                let qos = qos.min(2);
                let mut body = Vec::new();
                put_str(&mut body, topic.as_bytes());
                if qos > 0 {
                    body.extend_from_slice(&id().to_be_bytes());
                }
                body.extend_from_slice(&payload);
                if let Err(e) = conn.send(PUBLISH | (qos << 1) | u8::from(retain), &body) {
                    return lost(e);
                }
                last_sent = Instant::now();
            }
            Ok(Command::Close) => {
                let _ = conn.send(DISCONNECT, &[]);
                emit(Event::Closed("Disconnected".into()));
                return End::Stop;
            }
            Err(TryRecvError::Disconnected) => {
                let _ = conn.send(DISCONNECT, &[]);
                return End::Stop;
            }
            Ok(_) | Err(TryRecvError::Empty) => {}
        }
        if !keep_alive.is_zero() {
            if last_heard.elapsed() > keep_alive + keep_alive / 2 {
                return lost("Broker stopped responding".into());
            }
            if last_sent.elapsed() >= keep_alive / 2 {
                if let Err(e) = conn.send(PINGREQ, &[]) {
                    return lost(e);
                }
                last_sent = Instant::now();
            }
        }
        let (header, body) = match conn.read() {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => return lost(e),
        };
        last_heard = Instant::now();
        let reply = match header >> 4 {
            3 => {
                let Some((topic, pid, payload)) = parse_publish(header, &body) else {
                    return lost("Malformed PUBLISH from broker".into());
                };
                let (text, binary) = match std::str::from_utf8(payload) {
                    Ok(text) => (text.to_string(), false),
                    Err(_) => (hex(payload), true),
                };
                let event = if header & 1 == 1 { format!("{topic} (retained)") } else { topic };
                emit(Event::Received { text, binary, event: Some(event) });
                match ((header >> 1) & 3, pid) {
                    (1, Some(pid)) => Some((PUBACK, pid)),
                    (2, Some(pid)) => Some((PUBREC, pid)),
                    _ => None,
                }
            }
            5 => take_u16(&body, 0).map(|pid| (PUBREL, pid)),
            6 => take_u16(&body, 0).map(|pid| (PUBCOMP, pid)),
            9 => {
                let refused: Vec<&str> = topics
                    .iter()
                    .zip(body.get(2..).unwrap_or_default())
                    .filter(|(_, code)| **code == 0x80)
                    .map(|(t, _)| t.as_str())
                    .collect();
                let text = if refused.is_empty() {
                    format!("Subscribed to {}", topics.join(", "))
                } else {
                    format!("Broker refused subscription to {}", refused.join(", "))
                };
                emit(Event::Received { text, binary: false, event: Some("SUBACK".into()) });
                None
            }
            _ => None, // PUBACK, PUBCOMP, PINGRESP
        };
        if let Some((kind, pid)) = reply {
            if let Err(e) = conn.send(kind, &pid.to_be_bytes()) {
                return lost(e);
            }
            last_sent = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MQTT;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    fn read_packet(s: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut head = [0_u8; 1];
        s.read_exact(&mut head).unwrap();
        let (mut len, mut shift) = (0_usize, 0);
        loop {
            let mut b = [0_u8; 1];
            s.read_exact(&mut b).unwrap();
            len += ((b[0] & 0x7F) as usize) << shift;
            shift += 7;
            if b[0] & 0x80 == 0 {
                break;
            }
        }
        let mut body = vec![0; len];
        s.read_exact(&mut body).unwrap();
        (head[0], body)
    }

    fn next(rx: &mut futures::channel::mpsc::UnboundedReceiver<Event>) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(event) => return event,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(_) => panic!("timed out waiting for an MQTT event"),
            }
        }
    }

    #[test]
    fn encodes_remaining_length() {
        assert_eq!(packet(PINGREQ, &[]), vec![0xC0, 0]);
        let big = packet(PUBLISH, &[0; 321]);
        assert_eq!(&big[..3], &[0x30, 0xC1, 0x02]);
        assert_eq!(remaining_length(&big).unwrap(), Some((321, 3)));
        assert_eq!(remaining_length(&[0x30]).unwrap(), None);
    }

    #[test]
    fn connects_subscribes_publishes_and_receives() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let (head, body) = read_packet(&mut s);
            assert_eq!(head, CONNECT);
            assert_eq!(&body[..7], b"\0\x04MQTT\x04");
            assert_eq!(body[7], 0x02 | 0x80 | 0x40, "clean session with username and password");
            assert!(body.windows(5).any(|w| w == b"alice"));
            s.write_all(&[0x20, 2, 0, 0]).unwrap();

            let (head, body) = read_packet(&mut s);
            assert_eq!(head, SUBSCRIBE);
            assert_eq!(&body[2..], b"\0\x09sensors/#\x01");
            s.write_all(&[0x90, 3, body[0], body[1], 1]).unwrap();

            // A retained QoS 1 message from the broker must be acknowledged.
            let mut publish = Vec::new();
            put_str(&mut publish, b"sensors/t");
            publish.extend_from_slice(&7_u16.to_be_bytes());
            publish.extend_from_slice(b"21.5");
            s.write_all(&packet(PUBLISH | 0x02 | 1, &publish)).unwrap();
            assert_eq!(read_packet(&mut s), (PUBACK, vec![0, 7]));

            let (head, body) = read_packet(&mut s);
            assert_eq!(head, PUBLISH | 0x02 | 1, "QoS 1, retained");
            assert_eq!(parse_publish(head, &body).unwrap().2, b"on");
            assert_eq!(read_packet(&mut s).0, DISCONNECT);
        });

        let mut request = Request::default();
        request.set_url(format!("mqtt://127.0.0.1:{port}"));
        assert_eq!(request.method, MQTT);
        request.auth = Auth::Basic { username: "alice".into(), password: "{{pw}}".into() };
        request.socket.subscribe = "sensors/#".into();
        request.socket.qos = 1;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, mut ev_rx) = futures::channel::mpsc::unbounded();
        let vars = vec![("pw".to_string(), "secret".to_string())];
        let worker = thread::spawn(move || crate::ws::run(&request, &vars, cmd_rx, ev_tx));

        assert!(matches!(next(&mut ev_rx), Event::Connected(status, _) if status == "CONNACK accepted"));
        assert_eq!(
            next(&mut ev_rx),
            Event::Received { text: "Subscribed to sensors/#".into(), binary: false, event: Some("SUBACK".into()) }
        );
        assert_eq!(
            next(&mut ev_rx),
            Event::Received { text: "21.5".into(), binary: false, event: Some("sensors/t (retained)".into()) }
        );
        cmd_tx.send(Command::Publish { topic: "lights".into(), payload: b"on".to_vec(), qos: 1, retain: true }).unwrap();
        cmd_tx.send(Command::Close).unwrap();
        assert!(matches!(next(&mut ev_rx), Event::Closed(_)));
        worker.join().unwrap();
        broker.join().unwrap();
    }

    /// Round trip through a public broker, plain and TLS: `cargo test -- --ignored public_broker`.
    #[test]
    #[ignore = "needs network"]
    fn public_broker() {
        for url in ["mqtt://test.mosquitto.org", "mqtts://test.mosquitto.org:8886"] {
            let topic = format!("floki-test/{}", std::process::id());
            let mut request = Request::default();
            request.set_url(url.into());
            request.socket.subscribe = topic.clone();
            request.socket.qos = 1;
            let (cmd_tx, cmd_rx) = mpsc::channel();
            let (ev_tx, mut ev_rx) = futures::channel::mpsc::unbounded();
            let worker = thread::spawn(move || crate::ws::run(&request, &[], cmd_rx, ev_tx));
            assert!(matches!(next(&mut ev_rx), Event::Connected(..)), "{url}");
            assert!(matches!(next(&mut ev_rx), Event::Received { event: Some(e), .. } if e == "SUBACK"));
            cmd_tx.send(Command::Publish { topic: topic.clone(), payload: b"hello".to_vec(), qos: 1, retain: false }).unwrap();
            assert_eq!(next(&mut ev_rx), Event::Received { text: "hello".into(), binary: false, event: Some(topic) });
            cmd_tx.send(Command::Close).unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn reports_refused_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_packet(&mut s);
            s.write_all(&[0x20, 2, 0, 4]).unwrap();
        });
        let mut request = Request::default();
        request.set_url(format!("mqtt://127.0.0.1:{port}"));
        let (_cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, mut ev_rx) = futures::channel::mpsc::unbounded();
        let worker = thread::spawn(move || crate::ws::run(&request, &[], cmd_rx, ev_tx));
        assert!(matches!(next(&mut ev_rx), Event::Failed(e) if e.contains("bad username or password")));
        worker.join().unwrap();
        broker.join().unwrap();
    }
}
