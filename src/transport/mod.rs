//! WebSocket transport module for telemetry reports and remote configuration.
//!
//! Submodules:
//! - [`frames`]: Protocol frame types and config payload unpacking
//! - [`schedule`]: Schedule inactivation checks and backoff delays
//! - [`url`]: WebSocket URL transformations

pub mod frames;
pub mod schedule;
pub mod url;

pub use frames::{
    wss_config_body_and_headers, wss_config_payload, wss_string_from_value, WsHelloFrame,
    WsServerFrame,
};
pub use schedule::{
    is_auth_config_http_status, is_wss_schedule_inactive_reason, next_wss_backoff,
    wss_schedule_inactive_from_frame, wss_schedule_inactive_from_handshake,
    wss_schedule_inactive_from_headers, AGENT_WSS_MODE_ACTIVE, AGENT_WSS_MODE_DISABLED,
    AGENT_WSS_MODE_HEADER, AGENT_WSS_MODE_INACTIVE, AGENT_WSS_REASON_HEADER,
    AGENT_WSS_SCHEDULE_DISABLED, AGENT_WSS_SCHEDULE_EMPTY, AGENT_WSS_SCHEDULE_INACTIVE,
    WSS_NETWORK_MAX_RETRY, WSS_NETWORK_MIN_RETRY, WSS_PROTOCOL_RETRY_DELAY,
};
pub use url::{report_websocket_url, report_websocket_url_with_config};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::net::dns::use_public_dns_resolver;
use crate::net::websocket::{dial_websocket, WebSocketConn, WsError, WS_OPCODE_TEXT};
use crate::types::{Config, CONFIG_SCHEMA_VERSION, DEFAULT_WSS_REPORT_INTERVAL_SEC};
use crate::util::log::Logger;

pub const WSS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WSS_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
pub const WSS_WRITE_TIMEOUT: Duration = Duration::from_secs(8);
pub const WSS_READ_IDLE_GRACE: Duration = Duration::from_secs(15);
pub const WSS_READ_IDLE_MIN: Duration = Duration::from_secs(15);
pub const WSS_CONFIG_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub const WSS_DYNAMIC_MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const WSS_DYNAMIC_MAX_INTERVAL: Duration = Duration::from_secs(300);

pub trait TransportHost: Send + Sync {
    fn logger(&self) -> Logger;
    fn config_snapshot(&self) -> Config;
    fn version(&self) -> String;
    fn uses_wss(&self) -> bool;
    fn calibrate_date(&self, date_str: &str, rtt: Duration);
    fn note_report_failure(&self, status_code: i32);
    fn note_report_success(&self);
    fn disable_wss_runtime(&self, reason: &str);
    fn apply_wss_remote_config(&self, body: &[u8], headers: &HashMap<String, String>) -> Result<(), String>;
    fn wake_tick(&self);
}

struct Inner {
    ws_url: String,
    pause_until: Option<Instant>,
    pause_reason: String,
    last_config_at: Option<Instant>,
    next_report_after: Option<Duration>,
    running: bool,
    stopped: bool,
    restart_requested: bool,
}

pub struct ReportTransport {
    host: Arc<dyn TransportHost>,
    inner: Arc<Mutex<Inner>>,
    conn: Arc<Mutex<Option<WebSocketConn>>>,
    connected: Arc<AtomicBool>,
    wake_cv: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
}

impl ReportTransport {
    pub fn new(host: Arc<dyn TransportHost>) -> Self {
        let cfg = host.config_snapshot();
        let ws_url = match report_websocket_url(&cfg.worker_url) {
            Ok(u) => u,
            Err(e) => {
                host.logger().info(&format!("WSS retry delayed reason=invalid_url error={}", e));
                String::new()
            }
        };

        Self {
            host,
            inner: Arc::new(Mutex::new(Inner {
                ws_url,
                pause_until: None,
                pause_reason: String::new(),
                last_config_at: None,
                next_report_after: None,
                running: false,
                stopped: false,
                restart_requested: false,
            })),
            conn: Arc::new(Mutex::new(None)),
            connected: Arc::new(AtomicBool::new(false)),
            wake_cv: Arc::new(Condvar::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn url(&self) -> String {
        self.inner.lock().unwrap().ws_url.clone()
    }

    pub fn report_interval(&self, fallback: Duration) -> Duration {
        self.inner.lock().unwrap().next_report_after.unwrap_or(fallback)
    }

    pub fn set_next_report_after_ms(&self, ms: i64) {
        if ms <= 0 {
            return;
        }
        let mut next = Duration::from_millis(ms as u64);
        if next < WSS_DYNAMIC_MIN_INTERVAL {
            next = WSS_DYNAMIC_MIN_INTERVAL;
        }
        if next > WSS_DYNAMIC_MAX_INTERVAL {
            next = WSS_DYNAMIC_MAX_INTERVAL;
        }
        let mut inner = self.inner.lock().unwrap();
        let previous = inner.next_report_after;
        inner.next_report_after = Some(next);
        let should_wake = previous.is_none() || Some(next) < previous;
        drop(inner);

        if should_wake {
            self.host.wake_tick();
        }
    }

    pub fn reset_report_interval(&self) {
        self.inner.lock().unwrap().next_report_after = None;
    }

    pub fn start(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.ws_url.is_empty() {
            return false;
        }
        if inner.running {
            let was_stopped = inner.stopped;
            inner.stopped = false;
            if was_stopped {
                inner.restart_requested = true;
                self.wake_cv.notify_all();
            }
            return false;
        }
        inner.running = true;
        inner.stopped = false;
        drop(inner);

        let this = self.clone();
        std::thread::Builder::new()
            .name("wss-transport".to_string())
            .spawn(move || {
                this.run();
            })
            .ok();
        true
    }

    pub fn stop(&self, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.stopped = true;
        inner.pause_until = None;
        inner.pause_reason.clear();
        drop(inner);

        let mut guard = self.conn.lock().unwrap();
        let _ = guard.take();
        self.connected.store(false, AtomicOrdering::SeqCst);

        self.wake_cv.notify_all();
        if !reason.is_empty() {
            self.host.logger().info(&format!("WSS stopped reason={}", reason));
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.lock().unwrap().stopped
    }

    pub fn connected(&self) -> bool {
        self.connected.load(AtomicOrdering::SeqCst)
    }

    pub fn send(&self, body: &[u8]) -> bool {
        if !self.connected.load(AtomicOrdering::SeqCst) {
            return false;
        }
        {
            let inner = self.inner.lock().unwrap();
            if let Some(until) = inner.pause_until {
                if Instant::now() < until {
                    return false;
                }
            }
        }
        let mut guard = self.conn.lock().unwrap();
        if let Some(conn) = guard.as_mut() {
            if conn.write_text(body, WSS_WRITE_TIMEOUT).is_ok() {
                return true;
            }
            let _ = guard.take();
            self.connected.store(false, AtomicOrdering::SeqCst);
        }
        false
    }

    pub fn delay_protocol(&self, reason: &str) {
        let r = if reason.is_empty() { "protocol_error" } else { reason };
        let mut inner = self.inner.lock().unwrap();
        inner.pause_until = Some(Instant::now() + WSS_PROTOCOL_RETRY_DELAY);
        inner.pause_reason = r.to_string();
        drop(inner);

        let mut guard = self.conn.lock().unwrap();
        let _ = guard.take();
        self.connected.store(false, AtomicOrdering::SeqCst);

        self.host.logger().info(&format!(
            "WSS retry delayed reason={} delay={}s",
            r,
            WSS_PROTOCOL_RETRY_DELAY.as_secs()
        ));
    }

    fn should_run(&self) -> bool {
        if self.shutdown.load(AtomicOrdering::Relaxed) || !self.host.uses_wss() {
            return false;
        }
        !self.is_stopped()
    }

    fn run(&self) {
        let ws_url = self.inner.lock().unwrap().ws_url.clone();
        if ws_url.is_empty() {
            return;
        }

        let mut backoff = WSS_NETWORK_MIN_RETRY;
        loop {
            if !self.should_run() {
                break;
            }
            self.clear_restart_request();

            if !self.wait_protocol_pause() {
                break;
            }
            if !self.should_run() {
                break;
            }
            self.clear_restart_request();

            let (conn, headers, started, received) = match self.dial() {
                Ok(res) => res,
                Err(err) => {
                    if let WsError::Handshake {
                        status_code,
                        headers,
                        body,
                    } = &err
                    {
                        if let Some(reason) = wss_schedule_inactive_from_handshake(*status_code, headers, body) {
                            self.host.disable_wss_runtime(&reason);
                            backoff = WSS_NETWORK_MIN_RETRY;
                            continue;
                        }
                        if is_auth_config_http_status(*status_code as i32) {
                            self.host.note_report_failure(*status_code as i32);
                            self.delay_protocol(&format!("WSS handshake http={}", status_code));
                            backoff = WSS_NETWORK_MIN_RETRY;
                            continue;
                        }
                    }
                    if !self.wait_network_retry(&err.to_string(), backoff) {
                        break;
                    }
                    backoff = next_wss_backoff(backoff);
                    continue;
                }
            };

            if !self.should_run() {
                break;
            }
            backoff = WSS_NETWORK_MIN_RETRY;

            if let Some(date) = headers.get("date") {
                let rtt = received.duration_since(started);
                self.host.calibrate_date(date, rtt);
            }

            let mut conn = conn;
            let hello = match self.wait_hello(&mut conn) {
                Ok(h) => h,
                Err(err) => {
                    if self.handle_connection_error(&err, &mut backoff) {
                        continue;
                    }
                    break;
                }
            };

            {
                let inner = self.inner.lock().unwrap();
                if inner.stopped || !self.host.uses_wss() {
                    break;
                }
            }
            {
                let mut guard = self.conn.lock().unwrap();
                *guard = Some(conn);
            }
            self.connected.store(true, AtomicOrdering::SeqCst);

            self.host.logger().info(&format!(
                "WSS connected url={} protocol={} ts={}",
                ws_url, hello.protocol, hello.ts
            ));
            self.host.note_report_success();
            self.host.wake_tick();

            let err = self.read_loop();
            {
                let mut guard = self.conn.lock().unwrap();
                let _ = guard.take();
            }
            self.connected.store(false, AtomicOrdering::SeqCst);
            self.host.wake_tick();

            if let Some(err) = err {
                if self.handle_connection_error(&err, &mut backoff) {
                    continue;
                }
            }
            break;
        }

        let mut inner = self.inner.lock().unwrap();
        inner.running = false;
        inner.restart_requested = false;
    }

    fn dial(&self) -> Result<(WebSocketConn, HashMap<String, String>, Instant, Instant), WsError> {
        let cfg = self.host.config_snapshot();
        let md5 = if cfg.config_md5.is_empty() {
            "none"
        } else {
            &cfg.config_md5
        };
        let ws_url = self.inner.lock().unwrap().ws_url.clone();
        let dial_url = report_websocket_url_with_config(&ws_url, CONFIG_SCHEMA_VERSION, md5)
            .unwrap_or(ws_url);

        let version = self.host.version();
        let req_id = format!("{:012x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros());
        let headers = [
            ("Accept", "*/*"),
            ("User-Agent", "cfsm"),
            ("X-Agent-Config-Schema", CONFIG_SCHEMA_VERSION),
            ("X-Agent-Version", version.as_str()),
            ("X-Agent-Config-Md5", md5),
            ("X-Request-Id", req_id.as_str()),
        ];

        dial_websocket(&dial_url, &headers, WSS_HANDSHAKE_TIMEOUT, use_public_dns_resolver(&cfg))
    }

    fn wait_hello(&self, conn: &mut WebSocketConn) -> Result<WsHelloFrame, WsError> {
        let _ = conn.set_read_timeout(Some(WSS_HELLO_TIMEOUT));
        let (payload, opcode) = conn.read_data_message()?;
        let _ = conn.set_read_timeout(None);

        if opcode != WS_OPCODE_TEXT {
            return Err(WsError::Protocol(format!("WSS hello invalid opcode={}", opcode)));
        }
        let hello: WsHelloFrame = serde_json::from_slice(&payload)
            .map_err(|e| WsError::Protocol(format!("WSS hello invalid json: {}", e)))?;
        if hello.r#type != "hello" || hello.protocol != "update" {
            return Err(WsError::Protocol(format!(
                "WSS hello invalid type={:?} protocol={:?}",
                hello.r#type, hello.protocol
            )));
        }
        Ok(hello)
    }

    fn read_loop(&self) -> Option<WsError> {
        let mut last_activity = Instant::now();
        let short_poll = Duration::from_millis(100);

        loop {
            if !self.should_run() {
                return None;
            }
            let idle_timeout = self.read_idle_timeout();
            if last_activity.elapsed() >= idle_timeout {
                return Some(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "WSS idle timeout",
                )));
            }

            let msg_res;
            {
                let mut guard = self.conn.lock().unwrap();
                if let Some(conn) = guard.as_mut() {
                    let _ = conn.set_read_timeout(Some(short_poll));
                    msg_res = Some(conn.read_data_message());
                } else {
                    return None;
                }
            }
            let (payload, opcode) = match msg_res {
                Some(Ok(res)) => {
                    last_activity = Instant::now();
                    res
                }
                Some(Err(WsError::Io(ref io_err)))
                    if io_err.kind() == std::io::ErrorKind::TimedOut
                        || io_err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    continue;
                }
                Some(Err(err)) => return Some(err),
                None => return None,
            };

            if opcode != WS_OPCODE_TEXT {
                self.host.logger().debug(&format!(
                    "WSS message ignored opcode={} bytes={}",
                    opcode,
                    payload.len()
                ));
                continue;
            }

            let frame: WsServerFrame = match serde_json::from_slice(&payload) {
                Ok(f) => f,
                Err(e) => {
                    self.host.logger().debug(&format!("WSS message ignored invalid_json={}", e));
                    continue;
                }
            };

            match frame.r#type.as_str() {
                "ack" => {
                    let persisted = frame.persisted.unwrap_or(false);
                    let next_d1 = frame.next_d1_write_after_ms.unwrap_or(0);
                    let next_wss = frame.next_wss_report_after_ms.unwrap_or(0);
                    if let Some(ms) = frame.next_wss_report_after_ms {
                        self.set_next_report_after_ms(ms);
                    }
                    self.host.logger().debug(&format!(
                        "WSS ack ts={} persisted={} nextD1WriteAfterMs={} nextWssReportAfterMs={}",
                        frame.ts, persisted, next_d1, next_wss
                    ));
                    if !frame.realtime_hint {
                        self.host.note_report_success();
                    }
                    if let Some((body, headers)) = wss_config_body_and_headers(&frame) {
                        self.apply_config_body(&body, &headers);
                    }
                }
                "error" => {
                    let reason = if frame.error.is_empty() {
                        "server_error"
                    } else {
                        &frame.error
                    };
                    if let Some(schedule_reason) = wss_schedule_inactive_from_frame(&frame) {
                        self.host.logger().info(&format!(
                            "WSS unavailable ts={} reason={}",
                            frame.ts, schedule_reason
                        ));
                        self.host.disable_wss_runtime(&schedule_reason);
                        return None;
                    }
                    self.host.logger().error_once(
                        &format!("wss-server-err-{}", frame.code),
                        &format!(
                            "WSS received server error frame ts={} code={} error={}",
                            frame.ts, frame.code, reason
                        ),
                    );
                    return Some(WsError::Protocol(format!(
                        "server error code={} error={}",
                        frame.code, reason
                    )));
                }
                "config" | "remote_config" => {
                    if let Some((body, headers)) = wss_config_body_and_headers(&frame) {
                        self.apply_config_body(&body, &headers);
                    } else {
                        self.host.logger().debug("WSS config ignored: empty body");
                    }
                }
                "hello" => {
                    self.host.logger().debug(&format!("WSS hello repeated ts={}", frame.ts));
                }
                _ => {
                    self.host.logger().debug(&format!("WSS message ignored type={:?}", frame.r#type));
                }
            }
        }
    }

    fn read_idle_timeout(&self) -> Duration {
        let interval = self.report_interval(Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC as u64));
        let timeout = interval + WSS_READ_IDLE_GRACE;
        if timeout < WSS_READ_IDLE_MIN {
            WSS_READ_IDLE_MIN
        } else {
            timeout
        }
    }

    fn apply_config_body(&self, body: &[u8], headers: &HashMap<String, String>) {
        if !self.reserve_config_apply_slot() {
            return;
        }
        if let Err(e) = self.host.apply_wss_remote_config(body, headers) {
            self.host.logger().warn_once(
                &format!("wss-config:{}", e),
                &format!("WSS config rejected: {}", e),
            );
            return;
        }
        self.host.logger().info(&format!("WSS config processed bytes={}", body.len()));
    }

    fn reserve_config_apply_slot(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        if let Some(last) = inner.last_config_at {
            if now.duration_since(last) < WSS_CONFIG_MIN_INTERVAL {
                let remaining = WSS_CONFIG_MIN_INTERVAL - now.duration_since(last);
                self.host.logger().debug(&format!(
                    "WSS config delayed remaining={}s",
                    remaining.as_secs()
                ));
                return false;
            }
        }
        inner.last_config_at = Some(now);
        true
    }

    fn clear_restart_request(&self) {
        self.inner.lock().unwrap().restart_requested = false;
    }

    fn wait_protocol_pause(&self) -> bool {
        loop {
            let delay = {
                let inner = self.inner.lock().unwrap();
                if let Some(until) = inner.pause_until {
                    let now = Instant::now();
                    if now < until {
                        Some(until.duration_since(now))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            match delay {
                Some(d) => {
                    if !self.sleep(d) {
                        return false;
                    }
                }
                None => return true,
            }
        }
    }

    fn wait_network_retry(&self, err_msg: &str, delay: Duration) -> bool {
        let d = if delay < WSS_NETWORK_MIN_RETRY {
            WSS_NETWORK_MIN_RETRY
        } else {
            delay
        };
        self.host.logger().error_once(&format!("wss-retry-{}", err_msg), &format!(
            "WSS connection failed, retrying reason={} delay={}s",
            err_msg,
            d.as_secs()
        ));
        self.sleep(d)
    }

    fn handle_connection_error(&self, err: &WsError, backoff: &mut Duration) -> bool {
        if let WsError::Close { code, reason } = err {
            if *code == 1013 && is_wss_schedule_inactive_reason(reason) {
                self.host.logger().info(&format!("WSS unavailable close reason={}", reason));
                self.host.disable_wss_runtime(reason);
                return false;
            }
            if *code == 1008 {
                self.host.note_report_failure(*code as i32);
                let r = format!("WSS close code=1008 reason={}", reason);
                self.host.logger().error_once("wss-close-1008", &format!("WSS auth/protocol error: {}", r));
                self.delay_protocol(&r);
                *backoff = WSS_NETWORK_MIN_RETRY;
                return true;
            }
        }
        if let WsError::Protocol(msg) = err {
            if msg.starts_with("server error code=") {
                self.delay_protocol(msg);
                *backoff = WSS_NETWORK_MIN_RETRY;
                return true;
            }
        }
        if self.is_stopped() || !self.host.uses_wss() {
            return false;
        }
        let delay = *backoff;
        if !self.wait_network_retry(&err.to_string(), delay) {
            return false;
        }
        *backoff = next_wss_backoff(delay);
        true
    }

    fn sleep(&self, d: Duration) -> bool {
        if d.is_zero() {
            return true;
        }
        let inner = self.inner.lock().unwrap();
        let (guard, _timeout_res) = self.wake_cv.wait_timeout(inner, d).unwrap();
        if self.shutdown.load(AtomicOrdering::Relaxed) || guard.stopped {
            false
        } else {
            true
        }
    }
}

impl Clone for ReportTransport {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            inner: self.inner.clone(),
            conn: self.conn.clone(),
            connected: self.connected.clone(),
            wake_cv: self.wake_cv.clone(),
            shutdown: self.shutdown.clone(),
        }
    }
}
