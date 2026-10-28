#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition was not reached within five seconds"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "client did not connect");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.set_nodelay(true).unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
        assert!(request.len() < 16384);
    }
    let request = String::from_utf8(request).unwrap();
    let key = request
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim())
        })
        .unwrap();
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes());
    use base64::Engine;
    let accept = base64::engine::general_purpose::STANDARD.encode(digest);
    write!(stream, "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
    stream.flush().unwrap();
    stream
}

pub fn frame(opcode: u8, fin: bool, body: &[u8]) -> Vec<u8> {
    let mut frame = vec![opcode | if fin { 0x80 } else { 0 }];
    match body.len() {
        len @ 0..=125 => frame.push(len as u8),
        len @ 126..=65535 => {
            frame.push(126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(body);
    frame
}

pub fn send(stream: &mut TcpStream, value: &serde_json::Value) {
    stream
        .write_all(&frame(1, true, &serde_json::to_vec(value).unwrap()))
        .unwrap();
    stream.flush().unwrap();
}

pub fn hello(stream: &mut TcpStream, realtime: bool, interval_ms: u64) {
    send(
        stream,
        &serde_json::json!({
            "type": "hello", "protocol": "update", "realtimeHint": realtime,
            "nextWssReportAfterMs": interval_ms, "modeRevision": 1,
        }),
    );
}

pub fn read_json(stream: &mut TcpStream) -> serde_json::Value {
    loop {
        let mut header = [0; 2];
        stream.read_exact(&mut header).expect("client frame header");
        let len = match header[1] & 127 {
            126 => {
                let mut bytes = [0; 2];
                stream.read_exact(&mut bytes).unwrap();
                u16::from_be_bytes(bytes) as usize
            }
            127 => {
                let mut bytes = [0; 8];
                stream.read_exact(&mut bytes).unwrap();
                u64::from_be_bytes(bytes) as usize
            }
            len => len as usize,
        };
        assert!(len <= 1024 * 1024);
        assert_ne!(header[1] & 128, 0);
        let mut mask = [0; 4];
        stream.read_exact(&mut mask).unwrap();
        let mut body = vec![0; len];
        stream.read_exact(&mut body).unwrap();
        for (index, byte) in body.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        if header[0] & 15 == 1 {
            return serde_json::from_slice(&body).unwrap();
        }
    }
}
