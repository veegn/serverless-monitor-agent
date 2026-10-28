use serverless_monitor_agent::transport::{ReportTransport, TransportHost};
use serverless_monitor_agent::types::Config;
use serverless_monitor_agent::util::log::Logger;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModeUpdate {
    realtime: bool,
    interval_ms: u64,
    revision: u64,
    initial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportAck {
    report_id: String,
    accepted: bool,
    persisted: Option<bool>,
    accepted_samples: Option<usize>,
}

struct MockHost {
    config: Config,
    logger: Logger,
    mode_revision: Mutex<u64>,
    seen_modes: Mutex<Vec<ModeUpdate>>,
    applied_modes: Mutex<Vec<ModeUpdate>>,
    report_acks: Mutex<Vec<ReportAck>>,
}

impl MockHost {
    fn new(worker_url: String) -> Self {
        Self {
            config: Config {
                server_id: "adaptive-test-node".to_string(),
                secret: "test-secret".to_string(),
                worker_url,
                ..Default::default()
            },
            logger: Logger::new(false),
            mode_revision: Mutex::new(0),
            seen_modes: Mutex::new(Vec::new()),
            applied_modes: Mutex::new(Vec::new()),
            report_acks: Mutex::new(Vec::new()),
        }
    }
}

impl TransportHost for MockHost {
    fn logger(&self) -> Logger {
        self.logger.clone()
    }

    fn config_snapshot(&self) -> Config {
        self.config.clone()
    }

    fn version(&self) -> String {
        "integration-test".to_string()
    }

    fn uses_wss(&self) -> bool {
        true
    }

    fn calibrate_date(&self, _date_str: &str, _rtt: Duration) {}

    fn note_report_failure(&self, _status_code: i32) {}

    fn note_report_success(&self) {}

    fn disable_wss_runtime(&self, _reason: &str) {}

    fn apply_wss_remote_config(
        &self,
        _body: &[u8],
        _headers: &HashMap<String, String>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_wss_mode(
        &self,
        realtime: bool,
        interval_ms: u64,
        revision: u64,
        initial: bool,
    ) -> bool {
        let update = ModeUpdate {
            realtime,
            interval_ms,
            revision,
            initial,
        };
        self.seen_modes.lock().unwrap().push(update.clone());

        let mut current_revision = self.mode_revision.lock().unwrap();
        if !initial && (revision == 0 || revision <= *current_revision) {
            return false;
        }
        if revision > 0 {
            *current_revision = revision;
        }
        self.applied_modes.lock().unwrap().push(update);
        true
    }

    fn apply_wss_report_ack(
        &self,
        report_id: &str,
        accepted: bool,
        persisted: Option<bool>,
        accepted_samples: Option<usize>,
    ) {
        self.report_acks.lock().unwrap().push(ReportAck {
            report_id: report_id.to_string(),
            accepted,
            persisted,
            accepted_samples,
        });
    }

    fn wake_tick(&self) {}
}

#[test]
fn adaptive_handshake_applies_initial_mode_and_ignores_stale_mode_ack() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local WSS endpoint");
    let address = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        let mut stream = accept_websocket(listener);
        write_text_frame(
            &mut stream,
            br#"{"type":"hello","protocol":"update","realtimeHint":false,"nextWssReportAfterMs":60000,"modeRevision":1}"#,
        );
        write_text_frame(
            &mut stream,
            br#"{"type":"ack","ackFor":"mode","realtimeHint":true,"nextWssReportAfterMs":2000,"modeRevision":2}"#,
        );
        write_text_frame(
            &mut stream,
            br#"{"type":"ack","ackFor":"mode","realtimeHint":false,"nextWssReportAfterMs":60000,"modeRevision":1}"#,
        );
        std::thread::sleep(Duration::from_millis(400));
    });

    let host = Arc::new(MockHost::new(format!("http://{address}/update")));
    let transport = ReportTransport::new(host.clone());
    assert!(transport.start());
    wait_until(Duration::from_secs(3), || {
        host.seen_modes.lock().unwrap().len() == 3
    });

    let seen = host.seen_modes.lock().unwrap().clone();
    assert_eq!(seen[0].revision, 1);
    assert!(seen[0].initial);
    assert_eq!(seen[1].revision, 2);
    assert!(!seen[1].initial);
    assert_eq!(
        seen[2].revision, 1,
        "stale mode frame reaches revision guard"
    );

    let applied = host.applied_modes.lock().unwrap().clone();
    assert_eq!(applied.len(), 2);
    assert!(!applied[0].realtime);
    assert!(applied[1].realtime);
    assert_eq!(
        transport.report_interval(Duration::from_secs(60)),
        Duration::from_secs(2)
    );
    assert!(host.report_acks.lock().unwrap().is_empty());

    transport.stop("integration_test_complete");
    server.join().expect("local WSS server completed");
}

#[test]
fn adaptive_report_ack_preserves_report_id_and_batch_commit_fields() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local WSS endpoint");
    let address = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        let mut stream = accept_websocket(listener);
        write_text_frame(
            &mut stream,
            br#"{"type":"hello","protocol":"update","realtimeHint":true,"nextWssReportAfterMs":2000,"modeRevision":4}"#,
        );

        let body = read_client_text_frame(&mut stream);
        let report: serde_json::Value = serde_json::from_str(&body).expect("valid report JSON");
        assert_eq!(report["reportId"], "batch-report-42");
        assert_eq!(report["batch"], true);
        assert_eq!(report["samples"].as_array().unwrap().len(), 2);

        write_text_frame(
            &mut stream,
            br#"{"type":"ack","ackFor":"report","reportId":"batch-report-42","accepted":true,"persisted":true,"acceptedSamples":2}"#,
        );
        std::thread::sleep(Duration::from_millis(300));
    });

    let host = Arc::new(MockHost::new(format!("http://{address}/update")));
    let transport = ReportTransport::new(host.clone());
    assert!(transport.start());
    wait_until(Duration::from_secs(8), || transport.connected());

    let report_body = serde_json::json!({
        "id": "adaptive-test-node",
        "reportId": "batch-report-42",
        "batch": true,
        "samples": [
            {"ts": 1000, "metrics": {"cpu": "1.0"}},
            {"ts": 3000, "metrics": {"cpu": "2.0"}}
        ]
    });
    let body = serde_json::to_vec(&report_body).unwrap();
    assert!(transport.send(&body));

    wait_until(Duration::from_secs(8), || {
        !host.report_acks.lock().unwrap().is_empty()
    });
    assert_eq!(
        host.report_acks.lock().unwrap().as_slice(),
        &[ReportAck {
            report_id: "batch-report-42".to_string(),
            accepted: true,
            persisted: Some(true),
            accepted_samples: Some(2),
        }]
    );
    assert_eq!(
        host.applied_modes.lock().unwrap().len(),
        1,
        "report ACK must not be treated as a mode update"
    );

    transport.stop("integration_test_complete");
    server.join().expect("local WSS server completed");
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let started = Instant::now();
    while !predicate() && started.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(predicate(), "condition not reached within {timeout:?}");
}

fn accept_websocket(listener: TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().expect("accept WebSocket connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).expect("read HTTP handshake");
        request.push(byte[0]);
    }
    let request = String::from_utf8(request).expect("valid HTTP handshake");
    let key = request
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim())
        .expect("WebSocket key header");

    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        hasher.finalize(),
    );
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
    .expect("write HTTP upgrade response");
    stream.flush().unwrap();
    stream
}

fn write_text_frame(stream: &mut TcpStream, payload: &[u8]) {
    let mut frame = vec![0x81];
    match payload.len() {
        len @ 0..=125 => frame.push(len as u8),
        len @ 126..=65_535 => {
            frame.push(126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame).expect("write server text frame");
    stream.flush().unwrap();
}

fn read_client_text_frame(stream: &mut TcpStream) -> String {
    let mut header = [0u8; 2];
    stream
        .read_exact(&mut header)
        .expect("read client frame header");
    assert_eq!(header[0] & 0x0f, 0x1, "client should send a text frame");
    assert_ne!(header[1] & 0x80, 0, "client frame must be masked");

    let len_code = (header[1] & 0x7f) as usize;
    let len = match len_code {
        126 => {
            let mut extended = [0u8; 2];
            stream.read_exact(&mut extended).unwrap();
            u16::from_be_bytes(extended) as usize
        }
        127 => {
            let mut extended = [0u8; 8];
            stream.read_exact(&mut extended).unwrap();
            u64::from_be_bytes(extended) as usize
        }
        value => value,
    };
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask).expect("read client mask");
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .expect("read client frame payload");
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % mask.len()];
    }
    String::from_utf8(payload).expect("client payload is UTF-8 JSON")
}

#[path = "support/websocket.rs"]
mod ws;

#[test]
fn partial_headers_payloads_and_fragmented_messages_survive_poll_timeouts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (done, finish) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut stream = ws::accept(&listener);
        ws::hello(&mut stream, true, 2000);
        let ack = serde_json::to_vec(&serde_json::json!({
            "type":"ack", "ackFor":"report", "reportId":"fragmented-report",
            "accepted":true, "persisted":true, "acceptedSamples":2,
            "padding":"enough bytes to exercise the extended payload length path",
        }))
        .unwrap();
        let first = ws::frame(1, false, &ack[..ack.len() - 8]);
        // Split the header and then the payload across multiple socket polls.
        stream.write_all(&first[..1]).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        stream.write_all(&first[1..6]).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        stream.write_all(&first[6..]).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        stream.write_all(&ws::frame(9, true, b"ping")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        stream
            .write_all(&ws::frame(0, true, &ack[ack.len() - 8..]))
            .unwrap();
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let host = Arc::new(MockHost::new(format!("http://{address}/update")));
    let transport = ReportTransport::new(host.clone());
    transport.start();
    ws::wait_until(|| !host.report_acks.lock().unwrap().is_empty());
    assert_eq!(
        host.report_acks.lock().unwrap()[0].report_id,
        "fragmented-report"
    );
    assert_eq!(
        host.report_acks.lock().unwrap()[0].accepted_samples,
        Some(2)
    );
    transport.stop("test complete");
    done.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn enqueue_and_stop_do_not_wait_for_a_server_data_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (received, receive) = std::sync::mpsc::channel();
    let (done, finish) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut stream = ws::accept(&listener);
        ws::hello(&mut stream, true, 2000);
        received.send(ws::read_json(&mut stream)).unwrap();
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let host = Arc::new(MockHost::new(format!("http://{address}/update")));
    let transport = ReportTransport::new(host);
    transport.start();
    ws::wait_until(|| transport.connected());
    assert!(
        !transport.send(&vec![0; 2 * 1024 * 1024 + 1]),
        "outbound memory is bounded"
    );
    let start = Instant::now();
    assert!(transport.send(br#"{"reportId":"nonblocking-send"}"#));
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "enqueue waited for socket I/O"
    );
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(5)).unwrap()["reportId"],
        "nonblocking-send"
    );
    let start = Instant::now();
    transport.stop("test complete");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "stop waited for socket I/O"
    );
    done.send(()).unwrap();
    server.join().unwrap();
}
