use crate::dns::resolve_host;
use crate::http::create_tls_config;
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
        _ => return Err(WsError::Protocol(format!("unsupported WSS scheme {:?}", scheme))),
    };

    let host = u.host_str().ok_or_else(|| WsError::Protocol("missing host".into()))?;
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

    let tcp = tcp_stream.ok_or_else(|| WsError::Protocol(last_err))?;

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
    let parts: Vec<&str> = status_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Err(WsError::Protocol(format!("invalid HTTP status line: {:?}", status_line)));
    }
    let status_code: u16 = parts[1].parse().map_err(|_| WsError::Protocol("invalid status code".into()))?;

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

    let upgrade = resp_headers.get("upgrade").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let connection = resp_headers.get("connection").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if !upgrade.contains("websocket") || !connection.contains("upgrade") {
        return Err(WsError::Protocol("WSS handshake missing upgrade headers".into()));
    }

    let expected_accept = expected_websocket_accept(&key);
    let got_accept = resp_headers.get("sec-websocket-accept").map(|s| s.as_str()).unwrap_or("");
    if got_accept != expected_accept {
        return Err(WsError::Protocol("WSS handshake invalid accept".into()));
    }

    Ok((
        WebSocketConn {
            reader,
            write_mu: Mutex::new(()),
        },
        resp_headers,
        started,
        received,
    ))
}

impl WebSocketConn {
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

    pub fn read_data_message(&mut self) -> Result<(Vec<u8>, u8), WsError> {
        let mut message = Vec::new();
        let mut message_opcode = 0u8;

        loop {
            let (fin, opcode, payload) = self.read_frame()?;
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
                    if message_opcode != 0 {
                        return Err(WsError::Protocol("WSS fragmented message overlapped".into()));
                    }
                    message_opcode = opcode;
                    message.extend_from_slice(&payload);
                }
                WS_OPCODE_CONTINUATION => {
                    if message_opcode == 0 {
                        return Err(WsError::Protocol("WSS continuation without message".into()));
                    }
                    message.extend_from_slice(&payload);
                }
                _ => {
                    return Err(WsError::Protocol(format!("WSS unsupported opcode={}", opcode)));
                }
            }

            if message.len() > WEBSOCKET_MAX_MESSAGE_SIZE {
                return Err(WsError::Protocol("WSS message too large".into()));
            }

            if fin && (message_opcode == WS_OPCODE_TEXT || message_opcode == WS_OPCODE_BINARY) {
                return Ok((message, message_opcode));
            }
        }
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), WsError> {
        let mut header = [0u8; 2];
        self.reader.read_exact(&mut header)?;

        // Once the frame header has started, read the rest with a safe timeout
        let _ = self.set_read_timeout(Some(Duration::from_secs(5)));

        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0F;
        let masked = header[1] & 0x80 != 0;
        let mut len = (header[1] & 0x7F) as u64;

        if len == 126 {
            let mut ext = [0u8; 2];
            self.reader.read_exact(&mut ext)?;
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.reader.read_exact(&mut ext)?;
            len = u64::from_be_bytes(ext);
        }

        if len > WEBSOCKET_MAX_MESSAGE_SIZE as u64 {
            return Err(WsError::Protocol("WSS frame too large".into()));
        }

        let mut mask_key = [0u8; 4];
        if masked {
            self.reader.read_exact(&mut mask_key)?;
        }

        let mut payload = vec![0u8; len as usize];
        if len > 0 {
            self.reader.read_exact(&mut payload)?;
        }

        if masked {
            for i in 0..payload.len() {
                payload[i] ^= mask_key[i % 4];
            }
        }

        Ok((fin, opcode, payload))
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8], timeout: Duration) -> Result<(), WsError> {
        let _guard = self.write_mu.lock().unwrap();
        let _ = self.set_write_timeout(Some(timeout));

        let rng = ring::rand::SystemRandom::new();
        let mut mask_key = [0u8; 4];
        rng.fill(&mut mask_key).map_err(|_| WsError::Protocol("entropy error".into()))?;

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
    rng.fill(&mut key).map_err(|_| "entropy error".to_string())?;
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
