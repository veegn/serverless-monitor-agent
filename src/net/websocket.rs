use crate::net::dns::resolve_host;
use crate::net::http::create_tls_config;
use crate::net::socket_wait::SocketWake;
use ring::rand::SecureRandom;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use sha1::{Digest as Sha1Digest, Sha1};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const WS_OPCODE_CONTINUATION: u8 = 0x0;
pub const WS_OPCODE_TEXT: u8 = 0x1;
pub const WS_OPCODE_BINARY: u8 = 0x2;
pub const WS_OPCODE_CLOSE: u8 = 0x8;
pub const WS_OPCODE_PING: u8 = 0x9;
pub const WS_OPCODE_PONG: u8 = 0xA;

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const WEBSOCKET_MAX_MESSAGE_SIZE: usize = 1 << 20;

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;

#[derive(Debug)]
pub enum WsError {
    Handshake {
        status_code: u16,
        headers: HashMap<String, String>,
        body: String,
    },
    Close {
        code: u16,
        reason: String,
    },
    Io(std::io::Error),
    Protocol(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Handshake {
                status_code,
                headers: _,
                body,
            } => {
                if body.is_empty() {
                    write!(f, "WSS handshake http={}", status_code)
                } else {
                    write!(f, "WSS handshake http={} body={}", status_code, body.trim())
                }
            }
            WsError::Close { code, reason } => {
                if reason.is_empty() {
                    write!(f, "WSS close code={}", code)
                } else {
                    write!(f, "WSS close code={} reason={}", code, reason)
                }
            }
            WsError::Io(e) => write!(f, "WSS IO error: {}", e),
            WsError::Protocol(msg) => write!(f, "WSS protocol error: {}", msg),
        }
    }
}

impl std::error::Error for WsError {}

impl From<std::io::Error> for WsError {
    fn from(e: std::io::Error) -> Self {
        WsError::Io(e)
    }
}

#[allow(clippy::large_enum_variant)]
enum WsUnderlyingStream {
    Plain(TcpStream),
    Tls(StreamOwned<ClientConnection, TcpStream>),
}

impl Read for WsUnderlyingStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            WsUnderlyingStream::Plain(s) => s.read(buf),
            WsUnderlyingStream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for WsUnderlyingStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            WsUnderlyingStream::Plain(s) => s.write(buf),
            WsUnderlyingStream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            WsUnderlyingStream::Plain(s) => s.flush(),
            WsUnderlyingStream::Tls(s) => s.flush(),
        }
    }
}

pub struct WebSocketConn {
    reader: BufReader<WsUnderlyingStream>,
    write_mu: Mutex<()>,
    last_activity: Instant,
    incoming: Vec<u8>,
    message: Vec<u8>,
    message_opcode: u8,
}

pub fn dial_websocket(
    raw_url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    use_public_dns: bool,
) -> Result<(WebSocketConn, HashMap<String, String>, Instant, Instant), WsError> {
    let u = url::Url::parse(raw_url).map_err(|e| WsError::Protocol(e.to_string()))?;
    let scheme = u.scheme();
    let is_tls = match scheme {
        "wss" => true,
        "ws" => false,
        _ => {
            return Err(WsError::Protocol(format!(
                "unsupported WSS scheme {:?}",
                scheme
            )))
        }
    };

    let host = u
        .host_str()
        .ok_or_else(|| WsError::Protocol("missing host".into()))?;
    let port = u.port().unwrap_or(if is_tls { 443 } else { 80 });

    let started = Instant::now();
    let ips = resolve_host(host, use_public_dns).map_err(WsError::Protocol)?;

    let mut tcp_stream = None;
    let mut last_err = String::new();
    for ip in ips {
        let addr = SocketAddr::new(ip, port);
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                let _ = s.set_nodelay(true);
                tcp_stream = Some(s);
                break;
            }
            Err(e) => {
                last_err = format!("failed to connect to {}: {}", addr, e);
            }
        }
    }

    let tcp = tcp_stream.ok_or(WsError::Protocol(last_err))?;

    let mut stream = if is_tls {
        let tls_config = create_tls_config();
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| WsError::Protocol(format!("invalid TLS server name {}: {}", host, e)))?;
        let conn = ClientConnection::new(tls_config, server_name)
            .map_err(|e| WsError::Protocol(format!("TLS client error: {}", e)))?;
        WsUnderlyingStream::Tls(StreamOwned::new(conn, tcp))
    } else {
        WsUnderlyingStream::Plain(tcp)
    };

    let key = new_websocket_key().map_err(WsError::Protocol)?;
    let path_and_query = match u.query() {
        Some(q) => format!("{}?{}", u.path(), q),
        None => u.path().to_string(),
    };
    let path_str = if path_and_query.is_empty() {
        "/"
    } else {
        &path_and_query
    };

    let host_header = if (is_tls && port == 443) || (!is_tls && port == 80) {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };

    let mut req_str = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        path_str, host_header, key
    );

    for (k, v) in headers {
        req_str.push_str(&format!("{}: {}\r\n", k, v));
    }
    req_str.push_str("\r\n");

    stream.write_all(req_str.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;

    let received = Instant::now();
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(WsError::Protocol(format!(
            "invalid HTTP status line: {:?}",
            status_line
        )));
    }
    let status_code: u16 = parts[1]
        .parse()
        .map_err(|_| WsError::Protocol("invalid status code".into()))?;

    let mut resp_headers = HashMap::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            resp_headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    if status_code != 101 {
        let limit = resp_headers
            .get("content-length")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
            .min(1024);
        let mut body = String::new();
        if limit > 0 {
            let _ = reader.by_ref().take(limit).read_to_string(&mut body);
        }
        return Err(WsError::Handshake {
            status_code,
            headers: resp_headers,
            body,
        });
    }

    let upgrade = resp_headers
        .get("upgrade")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let connection = resp_headers
        .get("connection")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if !upgrade.contains("websocket") || !connection.contains("upgrade") {
        return Err(WsError::Protocol(
            "WSS handshake missing upgrade headers".into(),
        ));
    }

    let expected_accept = expected_websocket_accept(&key);
    let got_accept = resp_headers
        .get("sec-websocket-accept")
        .map(|s| s.as_str())
        .unwrap_or("");
    if got_accept != expected_accept {
        return Err(WsError::Protocol("WSS handshake invalid accept".into()));
    }

    Ok((
        WebSocketConn {
            reader,
            write_mu: Mutex::new(()),
            last_activity: Instant::now(),
            incoming: Vec::new(),
            message: Vec::new(),
            message_opcode: 0,
        },
        resp_headers,
        started,
        received,
    ))
}

impl WebSocketConn {
    pub fn last_activity(&self) -> Instant {
        self.last_activity
    }

    pub(crate) fn wait_readable(
        &mut self,
        wake: &SocketWake,
        timeout: Duration,
    ) -> std::io::Result<bool> {
        if !self.reader.buffer().is_empty() || self.buffered_frame_ready() {
            return Ok(true);
        }
        match self.reader.get_mut() {
            WsUnderlyingStream::Plain(socket) => wake.wait(socket, timeout),
            WsUnderlyingStream::Tls(stream) => {
                // One TLS record may contain several frames. Plaintext already
                // decoded by rustls must be consumed even when TCP is idle.
                match stream.conn.reader().into_first_chunk() {
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    _ => return Ok(true),
                }
                wake.wait(&stream.sock, timeout)
            }
        }
    }

    fn buffered_frame_ready(&self) -> bool {
        let bytes = &self.incoming;
        if bytes.len() < 2 {
            return false;
        }
        if bytes[0] & 0x70 != 0 {
            return true;
        }
        let code = bytes[1] & 0x7f;
        let extended = match code {
            126 => 2,
            127 => 8,
            _ => 0,
        };
        if bytes.len() < 2 + extended {
            return false;
        }
        let length = match code {
            126 => u16::from_be_bytes([bytes[2], bytes[3]]) as u64,
            127 => u64::from_be_bytes(bytes[2..10].try_into().unwrap()),
            value => value as u64,
        };
        if bytes[0] & 0x0f >= 8 && (bytes[0] & 0x80 == 0 || length > 125) {
            return true;
        }
        length > WEBSOCKET_MAX_MESSAGE_SIZE as u64
            || bytes.len()
                >= 2 + extended + (if bytes[1] & 0x80 != 0 { 4 } else { 0 }) + length as usize
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self.reader.get_ref() {
            WsUnderlyingStream::Plain(s) => s.set_read_timeout(timeout),
            WsUnderlyingStream::Tls(s) => s.get_ref().set_read_timeout(timeout),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self.reader.get_ref() {
            WsUnderlyingStream::Plain(s) => s.set_write_timeout(timeout),
            WsUnderlyingStream::Tls(s) => s.get_ref().set_write_timeout(timeout),
        }
    }

    pub fn write_text(&mut self, payload: &[u8], timeout: Duration) -> Result<(), WsError> {
        self.write_frame(WS_OPCODE_TEXT, payload, timeout)
    }

    pub fn write_ping(&mut self, payload: &[u8], timeout: Duration) -> Result<(), WsError> {
        self.write_frame(WS_OPCODE_PING, payload, timeout)
    }

    pub fn read_data_message(&mut self) -> Result<(Vec<u8>, u8), WsError> {
        loop {
            let (fin, opcode, payload) = self.read_frame()?;
            self.last_activity = Instant::now();
            match opcode {
                WS_OPCODE_PING => {
                    let _ = self.write_frame(WS_OPCODE_PONG, &payload, Duration::from_secs(8));
                    continue;
                }
                WS_OPCODE_PONG => {
                    continue;
                }
                WS_OPCODE_CLOSE => {
                    let (code, reason) = parse_websocket_close(&payload);
                    let _ = self.write_frame(WS_OPCODE_CLOSE, &payload, Duration::from_secs(8));
                    return Err(WsError::Close { code, reason });
                }
                WS_OPCODE_TEXT | WS_OPCODE_BINARY => {
                    if self.message_opcode != 0 {
                        return Err(WsError::Protocol(
                            "WSS fragmented message overlapped".into(),
                        ));
                    }
                    self.message_opcode = opcode;
                    self.message.extend_from_slice(&payload);
                }
                WS_OPCODE_CONTINUATION => {
                    if self.message_opcode == 0 {
                        return Err(WsError::Protocol("WSS continuation without message".into()));
                    }
                    self.message.extend_from_slice(&payload);
                }
                _ => {
                    return Err(WsError::Protocol(format!(
                        "WSS unsupported opcode={}",
                        opcode
                    )));
                }
            }

            if self.message.len() > WEBSOCKET_MAX_MESSAGE_SIZE {
                return Err(WsError::Protocol("WSS message too large".into()));
            }

            if fin
                && (self.message_opcode == WS_OPCODE_TEXT
                    || self.message_opcode == WS_OPCODE_BINARY)
            {
                let opcode = self.message_opcode;
                self.message_opcode = 0;
                return Ok((std::mem::take(&mut self.message), opcode));
            }
        }
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), WsError> {
        loop {
            if self.incoming.len() >= 2 {
                let header = &self.incoming;
                let fin = header[0] & 0x80 != 0;
                let opcode = header[0] & 0x0f;
                if header[0] & 0x70 != 0 {
                    return Err(WsError::Protocol(
                        "unexpected WebSocket extension bits".into(),
                    ));
                }
                let masked = header[1] & 0x80 != 0;
                let length_code = header[1] & 0x7f;
                let extended = match length_code {
                    126 => 2,
                    127 => 8,
                    _ => 0,
                };
                if header.len() >= 2 + extended {
                    let length = match length_code {
                        126 => u16::from_be_bytes([header[2], header[3]]) as u64,
                        127 => u64::from_be_bytes(header[2..10].try_into().unwrap()),
                        value => value as u64,
                    };
                    if length > WEBSOCKET_MAX_MESSAGE_SIZE as u64 {
                        return Err(WsError::Protocol("WSS frame too large".into()));
                    }
                    if opcode >= 8 && (!fin || length > 125) {
                        return Err(WsError::Protocol("invalid WebSocket control frame".into()));
                    }
                    let payload_start = 2 + extended + if masked { 4 } else { 0 };
                    let frame_end = payload_start + length as usize;
                    if header.len() >= frame_end {
                        let mut payload = header[payload_start..frame_end].to_vec();
                        if masked {
                            let mask = &header[payload_start - 4..payload_start];
                            for (index, byte) in payload.iter_mut().enumerate() {
                                *byte ^= mask[index % 4];
                            }
                        }
                        self.incoming.drain(..frame_end);
                        return Ok((fin, opcode, payload));
                    }
                }
            }
            let mut bytes = [0u8; 8192];
            let received = self.reader.read(&mut bytes)?;
            if received == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "WebSocket peer closed the connection",
                )
                .into());
            }
            self.incoming.extend_from_slice(&bytes[..received]);
            self.last_activity = Instant::now();
            // Both partial frame bytes and fragmented messages remain in the
            // connection when a socket poll times out.
        }
    }

    fn write_frame(
        &mut self,
        opcode: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), WsError> {
        let _guard = self.write_mu.lock().unwrap();
        let _ = self.set_write_timeout(Some(timeout));

        let rng = ring::rand::SystemRandom::new();
        let mut mask_key = [0u8; 4];
        rng.fill(&mut mask_key)
            .map_err(|_| WsError::Protocol("entropy error".into()))?;

        let payload_len = payload.len();
        let mut header = Vec::with_capacity(14);
        header.push(0x80 | opcode);

        if payload_len < 126 {
            header.push(0x80 | (payload_len as u8));
        } else if payload_len <= 0xffff {
            header.push(0x80 | 126);
            header.extend_from_slice(&(payload_len as u16).to_be_bytes());
        } else {
            header.push(0x80 | 127);
            header.extend_from_slice(&(payload_len as u64).to_be_bytes());
        }

        header.extend_from_slice(&mask_key);

        let mut masked = Vec::with_capacity(payload_len);
        for (i, &b) in payload.iter().enumerate() {
            masked.push(b ^ mask_key[i % 4]);
        }

        let writer = self.reader.get_mut();
        writer.write_all(&header)?;
        writer.write_all(&masked)?;
        writer.flush()?;
        Ok(())
    }
}

fn new_websocket_key() -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    let rng = ring::rand::SystemRandom::new();
    let mut key = [0u8; 16];
    rng.fill(&mut key)
        .map_err(|_| "entropy error".to_string())?;
    Ok(BASE64.encode(key))
}

fn expected_websocket_accept(key: &str) -> String {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    let result = hasher.finalize();
    BASE64.encode(result)
}

fn parse_websocket_close(payload: &[u8]) -> (u16, String) {
    if payload.len() < 2 {
        return (1005, String::new());
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    let reason = String::from_utf8_lossy(&payload[2..]).to_string();
    (code, reason)
}
