use serverless_monitor_agent::transport::{
    is_auth_config_http_status, is_wss_schedule_inactive_reason, next_wss_backoff,
    next_wss_reconnect_backoff, report_websocket_url, report_websocket_url_with_config,
    wss_config_payload, WSS_FAST_RETRY_INTERVAL,
};
use serverless_monitor_agent::types::Config;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn test_report_websocket_url_converts_http_update_url() {
    let cases = [
        ("https://example.com/update", "wss://example.com/update"),
        ("http://example.com/update?token=1", "ws://example.com/update?token=1"),
        ("wss://example.com/update", "wss://example.com/update"),
        ("ws://example.com/update", "ws://example.com/update"),
    ];

    for (raw, want) in cases {
        let got = report_websocket_url(raw).expect("valid url");
        assert_eq!(got, want, "report_websocket_url({:?})", raw);
    }
}

#[test]
fn test_report_websocket_url_with_config_adds_query_parameters() {
    let got = report_websocket_url_with_config(
        "https://example.com/update?token=1",
        "5",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .expect("valid url with config");

    let parsed = url::Url::parse(&got).expect("valid parsed url");
    assert_eq!(parsed.scheme(), "wss");
    let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(pairs.get("token").map(|s| s.as_str()), Some("1"));
    assert_eq!(pairs.get("config_schema").map(|s| s.as_str()), Some("5"));
    assert_eq!(
        pairs.get("config_md5").map(|s| s.as_str()),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
}

#[test]
fn test_next_wss_backoff_starts_at_sixty_seconds() {
    assert_eq!(next_wss_backoff(Duration::ZERO), Duration::from_secs(60));
    assert_eq!(
        next_wss_backoff(Duration::from_secs(60)),
        Duration::from_secs(120)
    );
}

#[test]
fn test_wss_fast_retry_interval_is_two_seconds() {
    assert_eq!(WSS_FAST_RETRY_INTERVAL, Duration::from_secs(2));
}

#[test]
fn test_next_wss_reconnect_backoff_ladder() {
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(1)), Duration::from_secs(2));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(2)), Duration::from_secs(4));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(4)), Duration::from_secs(8));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(8)), Duration::from_secs(16));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(16)), Duration::from_secs(30));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(30)), Duration::from_secs(60));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(60)), Duration::from_secs(120));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(120)), Duration::from_secs(240));
    assert_eq!(next_wss_reconnect_backoff(Duration::from_secs(240)), Duration::from_secs(300));
}

#[test]
fn test_is_auth_config_http_status() {
    assert!(is_auth_config_http_status(401));
    assert!(is_auth_config_http_status(403));
    assert!(is_auth_config_http_status(404));
    assert!(!is_auth_config_http_status(200));
    assert!(!is_auth_config_http_status(500));
}

#[test]
fn test_is_wss_schedule_inactive_reason() {
    assert!(is_wss_schedule_inactive_reason("wss_schedule_inactive"));
    assert!(is_wss_schedule_inactive_reason("wss_schedule_empty"));
    assert!(is_wss_schedule_inactive_reason("wss_disabled"));
    assert!(!is_wss_schedule_inactive_reason("active"));
}

#[test]
fn test_wss_config_payload_string() {
    let payload = serde_json::json!("collect_interval=0&report_interval=60");
    let (body, md5, _) = wss_config_payload(&payload);
    assert_eq!(body, "collect_interval=0&report_interval=60");
    assert!(md5.is_empty());
}

struct MockHost {
    config: Config,
    logger: serverless_monitor_agent::util::log::Logger,
    success_called: std::sync::atomic::AtomicBool,
}

impl serverless_monitor_agent::transport::TransportHost for MockHost {
    fn logger(&self) -> serverless_monitor_agent::util::log::Logger {
        self.logger.clone()
    }
    fn config_snapshot(&self) -> Config {
        self.config.clone()
    }
    fn version(&self) -> String {
        "1.0.0".to_string()
    }
    fn uses_wss(&self) -> bool {
        true
    }
    fn calibrate_date(&self, _date_str: &str, _rtt: Duration) {}
    fn note_report_failure(&self, _status_code: i32) {}
    fn note_report_success(&self) {
        self.success_called.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn disable_wss_runtime(&self, _reason: &str) {}
    fn apply_wss_remote_config(
        &self,
        _body: &[u8],
        _headers: &std::collections::HashMap<String, String>,
    ) -> Result<(), String> {
        Ok(())
    }
    fn wake_tick(&self) {}
}

#[test]
fn test_report_transport_nonblocking_and_realtime_send() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let server_handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept connection");
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).expect("read handshake");
        let req = String::from_utf8_lossy(&buf[..n]);

        // Find Sec-WebSocket-Key
        let mut key = "";
        for line in req.lines() {
            if line.to_ascii_lowercase().starts_with("sec-websocket-key:") {
                key = line.split(':').nth(1).unwrap().trim();
                break;
            }
        }
        assert!(!key.is_empty(), "must have sec-websocket-key");

        // Calculate accept
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(key.as_bytes());
        hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let accept = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            hasher.finalize(),
        );

        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept
        );
        stream.write_all(resp.as_bytes()).expect("write handshake 101");
        stream.flush().unwrap();

        // Send hello frame: opcode 0x1 (text), unmasked
        let hello_payload = b"{\"type\":\"hello\",\"ts\":1720000000000,\"protocol\":\"update\"}";
        let mut hello_frame = vec![0x81, hello_payload.len() as u8];
        hello_frame.extend_from_slice(hello_payload);
        stream.write_all(&hello_frame).expect("write hello frame");
        stream.flush().unwrap();

        // Read client metrics frame (masked)
        let mut client_frame_header = [0u8; 2];
        stream.read_exact(&mut client_frame_header).expect("read client frame header");
        assert_eq!(client_frame_header[0], 0x81, "must be text frame");
        assert!(client_frame_header[1] & 0x80 != 0, "client frame must be masked");
        let len = (client_frame_header[1] & 0x7f) as usize;
        let mut mask_key = [0u8; 4];
        stream.read_exact(&mut mask_key).expect("read mask key");
        let mut masked_payload = vec![0u8; len];
        stream.read_exact(&mut masked_payload).expect("read masked payload");
        for i in 0..len {
            masked_payload[i] ^= mask_key[i % 4];
        }
        let client_json = String::from_utf8(masked_payload).expect("valid utf8 from client");
        assert!(client_json.contains("\"test_metric\":true"), "client payload verification");

        // Send ack frame with dynamic interval
        let ack_payload = b"{\"type\":\"ack\",\"ts\":1720000001000,\"persisted\":true,\"nextWssReportAfterMs\":1234}";
        let mut ack_frame = vec![0x81, ack_payload.len() as u8];
        ack_frame.extend_from_slice(ack_payload);
        stream.write_all(&ack_frame).expect("write ack frame");
        stream.flush().unwrap();

        // Keep connection open briefly then close
        std::thread::sleep(Duration::from_millis(150));
    });

    let config = Config {
        server_id: "test-srv".to_string(),
        secret: "secret".to_string(),
        worker_url: format!("http://{}/update", addr),
        report_interval: 60,
        ..Default::default()
    };
    let host = Arc::new(MockHost {
        config,
        logger: serverless_monitor_agent::util::log::Logger::new(false),
        success_called: std::sync::atomic::AtomicBool::new(false),
    });

    let transport = serverless_monitor_agent::transport::ReportTransport::new(host.clone());
    assert!(!transport.connected(), "initially not connected");
    assert_eq!(
        transport.report_interval(Duration::from_secs(2)),
        Duration::from_secs(2)
    );

    transport.start();

    // Wait up to 2 seconds for connection
    let start = std::time::Instant::now();
    while !transport.connected() && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(transport.connected(), "transport must be connected");

    // Connected check is instantaneous (< 1 microsecond, no lock contention)
    let check_start = std::time::Instant::now();
    for _ in 0..1000 {
        let _ = transport.connected();
    }
    assert!(check_start.elapsed() < Duration::from_millis(5), "connected() must be lock-free and instant");

    // Send metrics over WSS
    let send_ok = transport.send(b"{\"test_metric\":true}");
    assert!(send_ok, "transport.send must succeed");

    // Wait up to 1 second for ack to update dynamic report interval
    let ack_start = std::time::Instant::now();
    while transport.report_interval(Duration::from_secs(2)) != Duration::from_millis(1234)
        && ack_start.elapsed() < Duration::from_secs(2)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        transport.report_interval(Duration::from_secs(2)),
        Duration::from_millis(1234),
        "dynamic report interval must be updated by ack frame"
    );

    transport.stop("test_done");
    assert!(!transport.connected(), "stopped transport must be disconnected");

    server_handle.join().expect("server thread finished");
}
