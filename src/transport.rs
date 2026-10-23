use crate::dns::use_public_dns_resolver;
use crate::log::Logger;
use crate::types::*;
use crate::websocket::{dial_websocket, WebSocketConn, WsError, WS_OPCODE_TEXT};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const WSS_PROTOCOL_RETRY_DELAY: Duration = Duration::from_secs(120);
pub const WSS_NETWORK_MIN_RETRY: Duration = Duration::from_secs(60);
pub const WSS_NETWORK_MAX_RETRY: Duration = Duration::from_secs(300);
pub const WSS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WSS_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
pub const WSS_WRITE_TIMEOUT: Duration = Duration::from_secs(8);
pub const WSS_READ_IDLE_GRACE: Duration = Duration::from_secs(15);
pub const WSS_READ_IDLE_MIN: Duration = Duration::from_secs(15);
pub const WSS_CONFIG_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub const WSS_DYNAMIC_MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const WSS_DYNAMIC_MAX_INTERVAL: Duration = Duration::from_secs(300);

pub const AGENT_WSS_MODE_HEADER: &str = "x-agent-wss-mode";
pub const AGENT_WSS_REASON_HEADER: &str = "x-agent-wss-reason";
pub const AGENT_WSS_MODE_ACTIVE: &str = "active";
pub const AGENT_WSS_MODE_INACTIVE: &str = "inactive";
pub const AGENT_WSS_MODE_DISABLED: &str = "disabled";
pub const AGENT_WSS_SCHEDULE_INACTIVE: &str = "wss_schedule_inactive";
pub const AGENT_WSS_SCHEDULE_EMPTY: &str = "wss_schedule_empty";
pub const AGENT_WSS_SCHEDULE_DISABLED: &str = "wss_disabled";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsHelloFrame {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WsServerFrame {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub persisted: Option<bool>,
    #[serde(default, rename = "nextD1WriteAfterMs")]
    pub next_d1_write_after_ms: Option<i64>,
    #[serde(default, rename = "nextWssReportAfterMs")]
    pub next_wss_report_after_ms: Option<i64>,
    #[serde(default, rename = "realtimeHint")]
    pub realtime_hint: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub connection_mode: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub config_body: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub config_md5: String,
    #[serde(default, rename = "configMd5")]
    pub config_md5_camel: String,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
}

pub fn report_websocket_url(raw: &str) -> Result<String, String> {
    let mut u = url::Url::parse(raw.trim()).map_err(|e| e.to_string())?;
    let host = u.host_str().ok_or_else(|| format!("missing host in {:?}", raw))?;
    if host.is_empty() {
        return Err(format!("missing host in {:?}", raw));
    }
    match u.scheme().to_ascii_lowercase().as_str() {
        "https" => {
            u.set_scheme("wss").map_err(|_| "failed to set wss scheme")?;
        }
        "http" => {
            u.set_scheme("ws").map_err(|_| "failed to set ws scheme")?;
        }
        "wss" | "ws" => {}
        other => return Err(format!("unsupported scheme {:?}", other)),
    }
    Ok(u.to_string())
}

pub fn report_websocket_url_with_config(raw: &str, schema: &str, md5: &str) -> Result<String, String> {
    let ws_url = report_websocket_url(raw)?;
    let mut u = url::Url::parse(&ws_url).map_err(|e| e.to_string())?;
    {
        let mut pairs = u.query_pairs_mut();
        if !schema.trim().is_empty() {
            pairs.append_pair("config_schema", schema.trim());
        }
        if !md5.trim().is_empty() {
            pairs.append_pair("config_md5", md5.trim());
        }
    }
    Ok(u.to_string())
}

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
    conn: Option<WebSocketConn>,
    pause_until: Option<Instant>,
    pause_reason: String,
    last_post_delay_log: Option<Instant>,
    last_config_at: Option<Instant>,
    next_report_after: Option<Duration>,
    running: bool,
    stopped: bool,
    restart_requested: bool,
}

pub struct ReportTransport {
    host: Arc<dyn TransportHost>,
    inner: Arc<Mutex<Inner>>,
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
                conn: None,
                pause_until: None,
                pause_reason: String::new(),
                last_post_delay_log: None,
                last_config_at: None,
                next_report_after: None,
                running: false,
                stopped: false,
                restart_requested: false,
            })),
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
        let _ = inner.conn.take();
        drop(inner);
        self.wake_cv.notify_all();
        if !reason.is_empty() {
            self.host.logger().info(&format!("WSS stopped reason={}", reason));
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.lock().unwrap().stopped
    }

    pub fn connected(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        if let Some(until) = inner.pause_until {
            if Instant::now() < until {
                return false;
            }
        }
        inner.conn.is_some()
    }

    pub fn send(&self, body: &[u8]) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if let Some(until) = inner.pause_until {
            if Instant::now() < until {
                return false;
            }
        }
        if let Some(conn) = inner.conn.as_mut() {
            if conn.write_text(body, WSS_WRITE_TIMEOUT).is_ok() {
                return true;
            }
            let _ = inner.conn.take();
        }
        false
    }

    pub fn post_fallback_allowed(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        if let Some(until) = inner.pause_until {
            Instant::now() >= until
        } else {
            true
        }
    }

    pub fn log_post_fallback_delayed(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let (delay, reason) = if let Some(until) = inner.pause_until {
            if now < until {
                (until.duration_since(now), inner.pause_reason.clone())
            } else {
                return;
            }
        } else {
            return;
        };

        if let Some(last_log) = inner.last_post_delay_log {
            if now.duration_since(last_log) < Duration::from_secs(30) {
                return;
            }
        }
        inner.last_post_delay_log = Some(now);
        drop(inner);

        self.host.logger().info(&format!(
            "POST fallback delayed reason={} remaining={}s",
            reason,
            delay.as_secs()
        ));
    }

    pub fn delay_protocol(&self, reason: &str) {
        let r = if reason.is_empty() { "protocol_error" } else { reason };
        let mut inner = self.inner.lock().unwrap();
        inner.pause_until = Some(Instant::now() + WSS_PROTOCOL_RETRY_DELAY);
        inner.pause_reason = r.to_string();
        let _ = inner.conn.take();
        inner.last_post_delay_log = Some(Instant::now());
        drop(inner);

        self.host.logger().info(&format!(
            "WSS retry delayed reason={} delay={}s",
            r,
            WSS_PROTOCOL_RETRY_DELAY.as_secs()
        ));
        self.host.logger().info(&format!(
            "POST fallback delayed reason={} delay={}s",
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
                let mut inner = self.inner.lock().unwrap();
                if inner.stopped || !self.host.uses_wss() {
                    break;
                }
                inner.conn = Some(conn);
            }

            self.host.logger().info(&format!(
                "WSS connected url={} protocol={} ts={}",
                ws_url, hello.protocol, hello.ts
            ));
            self.host.note_report_success();

            let err = self.read_loop();
            {
                let mut inner = self.inner.lock().unwrap();
                let _ = inner.conn.take();
            }
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
        loop {
            if !self.should_run() {
                return None;
            }
            let timeout = self.read_idle_timeout();

            let mut msg_res = None;
            {
                let mut inner = self.inner.lock().unwrap();
                if let Some(conn) = inner.conn.as_mut() {
                    let _ = conn.set_read_timeout(Some(timeout));
                    msg_res = Some(conn.read_data_message());
                }
            }
            let (payload, opcode) = match msg_res {
                Some(Ok(res)) => res,
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
            wake_cv: self.wake_cv.clone(),
            shutdown: self.shutdown.clone(),
        }
    }
}

pub fn is_auth_config_http_status(status_code: i32) -> bool {
    status_code == 401 || status_code == 403 || status_code == 404
}

pub fn is_wss_schedule_inactive_reason(reason: &str) -> bool {
    let r = reason.trim().to_ascii_lowercase();
    r == AGENT_WSS_SCHEDULE_INACTIVE || r == AGENT_WSS_SCHEDULE_EMPTY || r == AGENT_WSS_SCHEDULE_DISABLED
}

pub fn wss_schedule_inactive_from_handshake(
    status_code: u16,
    headers: &HashMap<String, String>,
    body: &str,
) -> Option<String> {
    if status_code != 409 {
        return None;
    }
    if let Some(reason) = wss_schedule_inactive_from_headers(headers) {
        return Some(reason);
    }
    let b = body.trim().to_ascii_lowercase();
    if is_wss_schedule_inactive_reason(&b) {
        return Some(b);
    }
    None
}

pub fn wss_schedule_inactive_from_headers(headers: &HashMap<String, String>) -> Option<String> {
    let mode = headers.get(AGENT_WSS_MODE_HEADER).map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    if !mode.is_empty() && mode != AGENT_WSS_MODE_INACTIVE && mode != AGENT_WSS_MODE_DISABLED {
        return None;
    }
    let reason = headers.get(AGENT_WSS_REASON_HEADER).map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    if is_wss_schedule_inactive_reason(&reason) {
        Some(reason)
    } else {
        None
    }
}

pub fn wss_schedule_inactive_from_frame(frame: &WsServerFrame) -> Option<String> {
    if frame.code != 409 {
        return None;
    }
    let reason = if !frame.text.is_empty() {
        &frame.text
    } else {
        &frame.error
    };
    if is_wss_schedule_inactive_reason(reason) {
        Some(reason.to_string())
    } else {
        None
    }
}

pub fn next_wss_backoff(current: Duration) -> Duration {
    if current < WSS_NETWORK_MIN_RETRY {
        return WSS_NETWORK_MIN_RETRY;
    }
    let next = current * 2;
    if next > WSS_NETWORK_MAX_RETRY {
        WSS_NETWORK_MAX_RETRY
    } else {
        next
    }
}

pub fn wss_config_body_and_headers(
    frame: &WsServerFrame,
) -> Option<(Vec<u8>, HashMap<String, String>)> {
    let mut body = if !frame.body.is_empty() {
        frame.body.clone()
    } else if !frame.config_body.is_empty() {
        frame.config_body.clone()
    } else if let Some(v) = &frame.config {
        wss_string_from_value(v)
    } else {
        String::new()
    };

    let mut md5 = if !frame.config_md5.is_empty() {
        frame.config_md5.clone()
    } else if !frame.config_md5_camel.is_empty() {
        frame.config_md5_camel.clone()
    } else {
        frame.md5.clone()
    };

    let mut headers = HashMap::new();
    if let Some(h) = &frame.headers {
        if let Some(map) = h.as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    headers.insert(k.clone(), s.to_string());
                }
            }
            if let Some(h_md5) = headers.get("X-Agent-Config-Md5").or_else(|| headers.get("x-agent-config-md5")) {
                if md5.is_empty() {
                    md5 = h_md5.clone();
                }
            }
        }
    }

    if body.is_empty() {
        if let Some(p) = &frame.payload {
            let (p_body, p_md5, p_headers) = wss_config_payload(p);
            if !p_body.is_empty() {
                body = p_body;
                if md5.is_empty() {
                    md5 = p_md5;
                }
                for (k, v) in p_headers {
                    headers.insert(k, v);
                }
            }
        }
    }

    if body.is_empty() {
        if let Some(c) = &frame.config {
            let (c_body, c_md5, c_headers) = wss_config_payload(c);
            if !c_body.is_empty() {
                body = c_body;
                if md5.is_empty() {
                    md5 = c_md5;
                }
                for (k, v) in c_headers {
                    headers.insert(k, v);
                }
            }
        }
    }

    if !md5.is_empty() {
        headers.insert("X-Agent-Config-Md5".to_string(), md5);
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some((trimmed.as_bytes().to_vec(), headers))
    }
}

pub fn wss_config_payload(payload: &serde_json::Value) -> (String, String, HashMap<String, String>) {
    match payload {
        serde_json::Value::String(s) => (s.trim().to_string(), String::new(), HashMap::new()),
        serde_json::Value::Object(map) => {
            let mut headers = HashMap::new();
            if let Some(h) = map.get("headers").and_then(|v| v.as_object()) {
                for (k, v) in h {
                    if let Some(s) = v.as_str() {
                        headers.insert(k.clone(), s.to_string());
                    }
                }
            }
            let body = map
                .get("body")
                .and_then(|v| v.as_str())
                .or_else(|| map.get("config").and_then(|v| v.as_str()))
                .unwrap_or("")
                .trim()
                .to_string();
            let md5 = map
                .get("config_md5")
                .or_else(|| map.get("configMd5"))
                .or_else(|| map.get("md5"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            if !body.is_empty() {
                return (body, md5, headers);
            }

            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            let keys = [
                "collect_interval", "report_interval", "wss_report_interval", "reset_day", "schema_version",
                "interface", "connection_mode", "ping_mode", "custom_ct", "custom_cu", "custom_cm",
                "custom_bd", "node_1", "node_2", "node_3", "node_4", "rx_correction", "tx_correction", "update",
            ];
            for key in keys {
                if let Some(val) = map.get(key) {
                    let s = wss_string_from_value(val);
                    serializer.append_pair(key, &s);
                }
            }
            (serializer.finish(), md5, headers)
        }
        _ => (String::new(), String::new(), HashMap::new()),
    }
}

pub fn wss_string_from_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => if *b { "1".to_string() } else { "0".to_string() },
        _ => String::new(),
    }
}
