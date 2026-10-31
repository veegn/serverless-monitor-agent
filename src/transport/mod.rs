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
    next_wss_reconnect_backoff, wss_schedule_inactive_from_frame,
    wss_schedule_inactive_from_handshake, wss_schedule_inactive_from_headers,
    AGENT_WSS_MODE_ACTIVE, AGENT_WSS_MODE_DISABLED, AGENT_WSS_MODE_HEADER, AGENT_WSS_MODE_INACTIVE,
    AGENT_WSS_REASON_HEADER, AGENT_WSS_SCHEDULE_DISABLED, AGENT_WSS_SCHEDULE_EMPTY,
    AGENT_WSS_SCHEDULE_INACTIVE, WSS_FAST_RETRY_INTERVAL, WSS_NETWORK_MAX_RETRY,
    WSS_NETWORK_MIN_RETRY, WSS_PROTOCOL_RETRY_DELAY,
};
pub use url::{report_websocket_url, report_websocket_url_with_config};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::net::dns::use_public_dns_resolver;
use crate::net::socket_wait::SocketWake;
use crate::net::websocket::{dial_websocket, WebSocketConn, WsError, WS_OPCODE_TEXT};
use crate::types::{Config, CONFIG_SCHEMA_VERSION, DEFAULT_WSS_REPORT_INTERVAL_SEC};
use crate::util::log::Logger;

pub const WSS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WSS_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
pub const WSS_WRITE_TIMEOUT: Duration = Duration::from_secs(8);
pub const WSS_READ_IDLE_GRACE: Duration = Duration::from_secs(45);
pub const WSS_READ_IDLE_MIN: Duration = Duration::from_secs(45);
pub const WSS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
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
    fn apply_wss_remote_config(
        &self,
        body: &[u8],
        headers: &HashMap<String, String>,
    ) -> Result<(), String>;
    fn apply_wss_mode(
        &self,
        _realtime: bool,
        _interval_ms: u64,
        _revision: u64,
        _initial: bool,
    ) -> bool {
        true
    }
    fn apply_wss_report_ack(
        &self,
        _report_id: &str,
        _accepted: bool,
        _persisted: Option<bool>,
        _accepted_samples: Option<usize>,
    ) {
    }
    fn handle_wss_message_too_big(&self) {}
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
    auth_failed: bool,
    server_overload_attempt: usize,
}

pub struct ReportTransport {
    host: Arc<dyn TransportHost>,
    inner: Arc<Mutex<Inner>>,
    outbound: Arc<Mutex<VecDeque<Vec<u8>>>>,
    socket_wake: Option<Arc<SocketWake>>,
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
                host.logger()
                    .info(&format!("WSS retry delayed reason=invalid_url error={}", e));
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
                auth_failed: false,
                server_overload_attempt: 0,
            })),
            outbound: Arc::new(Mutex::new(VecDeque::new())),
            socket_wake: SocketWake::new().ok().map(Arc::new),
            connected: Arc::new(AtomicBool::new(false)),
            wake_cv: Arc::new(Condvar::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn url(&self) -> String {
        self.inner.lock().unwrap().ws_url.clone()
    }

    pub fn report_interval(&self, fallback: Duration) -> Duration {
        self.inner
            .lock()
            .unwrap()
            .next_report_after
            .unwrap_or(fallback)
    }

    pub fn report_interval_peek(&self, fallback: Duration) -> Duration {
        self.inner
            .lock()
            .unwrap()
            .next_report_after
            .unwrap_or(fallback)
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
        let should_wake = previous != Some(next);
        drop(inner);

        self.host.logger().info(&format!(
            "WSS dynamic report interval received from server: {}ms (next report delay)",
            next.as_millis()
        ));

        if should_wake {
            self.host.wake_tick();
        }
    }

    pub fn reset_report_interval(&self) {
        self.inner.lock().unwrap().next_report_after = None;
    }

    pub fn start(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.ws_url.is_empty() || inner.auth_failed {
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

        self.outbound.lock().unwrap().clear();
        self.connected.store(false, AtomicOrdering::SeqCst);

        self.wake_cv.notify_all();
        self.notify_socket();
        if !reason.is_empty() {
            self.host
                .logger()
                .info(&format!("WSS stopped reason={}", reason));
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.lock().unwrap().stopped
    }

    pub fn connected(&self) -> bool {
        self.connected.load(AtomicOrdering::SeqCst)
    }

    /// Queue a frame for the connection thread. Acceptance is not a server ACK.
    pub fn send(&self, body: &[u8]) -> bool {
        const MAX_QUEUED_FRAMES: usize = 8;
        const MAX_QUEUED_BYTES: usize = 2 * 1024 * 1024;
        let inner = self.inner.lock().unwrap();
        if !self.connected()
            || inner.stopped
            || inner
                .pause_until
                .is_some_and(|until| Instant::now() < until)
        {
            return false;
        }
        let mut outbound = self.outbound.lock().unwrap();
        if outbound.len() >= MAX_QUEUED_FRAMES
            || outbound.iter().map(Vec::len).sum::<usize>() + body.len() > MAX_QUEUED_BYTES
        {
            return false;
        }
        outbound.push_back(body.to_vec());
        drop(outbound);
        drop(inner);
        self.notify_socket();
        true
    }

    pub fn delay_protocol(&self, reason: &str) {
        let r = if reason.is_empty() {
            "protocol_error"
        } else {
            reason
        };
        let mut inner = self.inner.lock().unwrap();
        inner.pause_until = Some(Instant::now() + WSS_PROTOCOL_RETRY_DELAY);
        inner.pause_reason = r.to_string();
        drop(inner);

        self.outbound.lock().unwrap().clear();
        self.connected.store(false, AtomicOrdering::SeqCst);
        self.notify_socket();

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

    fn notify_socket(&self) {
        if let Some(wake) = &self.socket_wake {
            wake.notify();
        }
    }

    fn run(&self) {
        let ws_url = self.inner.lock().unwrap().ws_url.clone();
        if ws_url.is_empty() {
            return;
        }

        let mut backoff = WSS_FAST_RETRY_INTERVAL;
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
                        if let Some(reason) =
                            wss_schedule_inactive_from_handshake(*status_code, headers, body)
                        {
                            let delay = std::cmp::max(backoff, Duration::from_secs(30));
                            self.host.logger().warn(&format!(
                                "WSS schedule inactive on server (status 409 reason={}), backing off for {}s before retry",
                                reason,
                                delay.as_secs()
                            ));
                            if !self.wait_network_retry(
                                &format!("schedule_inactive: {}", reason),
                                delay,
                            ) {
                                break;
                            }
                            backoff = next_wss_reconnect_backoff(delay);
                            continue;
                        }
                        if *status_code == 401 || *status_code == 403 {
                            self.host.note_report_failure(*status_code as i32);
                            self.host.logger().error(&format!(
                                "WSS authentication failed with HTTP {}; credentials must be corrected before reconnecting",
                                status_code
                            ));
                            let mut inner = self.inner.lock().unwrap();
                            inner.auth_failed = true;
                            inner.stopped = true;
                            drop(inner);
                            self.host.wake_tick();
                            break;
                        }
                        if is_auth_config_http_status(*status_code as i32) {
                            self.host.note_report_failure(*status_code as i32);
                            self.delay_protocol(&format!("WSS handshake http={}", status_code));
                            backoff = std::cmp::max(backoff, WSS_PROTOCOL_RETRY_DELAY);
                            continue;
                        }
                    }
                    if !self.wait_network_retry(&err.to_string(), backoff) {
                        break;
                    }
                    backoff = next_wss_reconnect_backoff(backoff);
                    continue;
                }
            };

            if !self.should_run() {
                break;
            }
            backoff = WSS_FAST_RETRY_INTERVAL;

            if let Some(date) = headers.get("date") {
                let rtt = received.duration_since(started);
                self.host.calibrate_date(date, rtt);
            }

            let mut conn = conn;
            self.reset_report_interval();
            let hello = match self.wait_hello(&mut conn) {
                Ok(h) => h,
                Err(err) => {
                    self.host.logger().warn(&format!(
                        "WSS wait_hello failed (error: {}), backing off in {}s",
                        err,
                        backoff.as_secs()
                    ));
                    if self.handle_connection_error(&err, &mut backoff) {
                        continue;
                    }
                    if !self.should_run() {
                        break;
                    }
                    continue;
                }
            };

            let realtime_hint = hello.realtime_hint.unwrap_or(true);
            let interval_ms = hello
                .next_wss_report_after_ms
                .unwrap_or(if realtime_hint { 2_000 } else { 60_000 })
                .clamp(1_000, 300_000);
            if self.host.apply_wss_mode(
                realtime_hint,
                interval_ms,
                hello.mode_revision.unwrap_or(0),
                true,
            ) {
                self.set_next_report_after_ms(interval_ms as i64);
            }

            {
                let inner = self.inner.lock().unwrap();
                if inner.stopped || !self.host.uses_wss() {
                    break;
                }
            }
            self.connected.store(true, AtomicOrdering::SeqCst);

            self.host.logger().info(&format!(
                "WSS connected url={} protocol={} ts={}",
                ws_url, hello.protocol, hello.ts
            ));
            self.host.note_report_success();
            self.host.wake_tick();

            let err = self.read_loop(&mut conn);
            self.connected.store(false, AtomicOrdering::SeqCst);
            self.outbound.lock().unwrap().clear();
            self.host.wake_tick();

            if !self.should_run() {
                self.host
                    .logger()
                    .info("WSS transport stopped per lifecycle request");
                break;
            }

            let effective_err = err.unwrap_or_else(|| {
                WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection dropped unexpectedly",
                ))
            });

            backoff = WSS_FAST_RETRY_INTERVAL;
            self.host.logger().warn(&format!(
                "WSS connection dropped (reason: {}), entering fast reconnect in {}s",
                effective_err,
                backoff.as_secs()
            ));

            if self.handle_connection_error(&effective_err, &mut backoff) {
                continue;
            }
            if !self.should_run() {
                break;
            }
            continue;
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
        let dial_url =
            report_websocket_url_with_config(&ws_url, CONFIG_SCHEMA_VERSION, md5).unwrap_or(ws_url);

        let version = self.host.version();
        let req_id = format!(
            "{:012x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );
        let headers = [
            ("Accept", "*/*"),
            ("User-Agent", "cfsm"),
            ("X-Agent-Config-Schema", CONFIG_SCHEMA_VERSION),
            ("X-Agent-Version", version.as_str()),
            ("X-Agent-Config-Md5", md5),
            ("X-Request-Id", req_id.as_str()),
        ];

        dial_websocket(
            &dial_url,
            &headers,
            WSS_HANDSHAKE_TIMEOUT,
            use_public_dns_resolver(&cfg),
        )
    }

    fn wait_hello(&self, conn: &mut WebSocketConn) -> Result<WsHelloFrame, WsError> {
        let _ = conn.set_read_timeout(Some(WSS_HELLO_TIMEOUT));
        let (payload, opcode) = conn.read_data_message()?;
        let _ = conn.set_read_timeout(None);

        if opcode != WS_OPCODE_TEXT {
            return Err(WsError::Protocol(format!(
                "WSS hello invalid opcode={}",
                opcode
            )));
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

    fn read_loop(&self, conn: &mut WebSocketConn) -> Option<WsError> {
        let mut last_activity = Instant::now();
        let mut last_ping = Instant::now();
        let mut event_wait = self.socket_wake.as_deref();
        let fragment_timeout = Duration::from_millis(25);
        let fallback_poll = Duration::from_millis(100);
        let _ = conn.set_read_timeout(Some(if event_wait.is_some() {
            fragment_timeout
        } else {
            fallback_poll
        }));

        loop {
            if !self.should_run() {
                return None;
            }

            last_activity = last_activity.max(conn.last_activity());
            // Socket I/O belongs exclusively to this thread. Producers only
            // hold the queue lock long enough to enqueue or dequeue one frame.
            loop {
                let body = self.outbound.lock().unwrap().pop_front();
                let Some(body) = body else { break };
                if let Err(error) = conn.write_text(&body, WSS_WRITE_TIMEOUT) {
                    return Some(error);
                }
            }

            let idle_timeout = self.read_idle_timeout();
            if last_activity.elapsed() >= idle_timeout {
                return Some(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "WSS idle timeout",
                )));
            }

            // RFC 6455 control Ping; Cloudflare responds without waking a hibernated DO.
            if last_ping.elapsed() >= WSS_HEARTBEAT_INTERVAL {
                if let Err(error) = conn.write_ping(b"", Duration::from_secs(4)) {
                    return Some(error);
                }
                last_ping = Instant::now();
            }

            if let Some(wake) = event_wait {
                let timeout = Duration::from_secs(1)
                    .min(WSS_HEARTBEAT_INTERVAL.saturating_sub(last_ping.elapsed()))
                    .min(idle_timeout.saturating_sub(last_activity.elapsed()));
                match conn.wait_readable(wake, timeout) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        self.host.logger().warn_once(
                            "wss-readiness",
                            &format!(
                                "socket readiness unavailable; using bounded polling: {}",
                                error
                            ),
                        );
                        event_wait = None;
                        let _ = conn.set_read_timeout(Some(fallback_poll));
                    }
                }
            }
            let msg_res = conn.read_data_message();
            let (payload, opcode) = match msg_res {
                Ok(res) => {
                    last_activity = Instant::now();
                    res
                }
                Err(WsError::Io(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::TimedOut
                        || io_err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    std::thread::yield_now();
                    continue;
                }
                Err(err) => return Some(err),
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
                    self.host
                        .logger()
                        .debug(&format!("WSS message ignored invalid_json={}", e));
                    continue;
                }
            };

            match frame.r#type.as_str() {
                "ack" => {
                    let next_d1 = frame.next_d1_write_after_ms.unwrap_or(0);
                    let next_wss = frame.next_wss_report_after_ms.unwrap_or(0);
                    self.host.logger().debug(&format!(
                        "WSS ack ts={} ackFor={:?} accepted={:?} persisted={:?} nextD1WriteAfterMs={} nextWssReportAfterMs={}",
                        frame.ts, frame.ack_for, frame.accepted, frame.persisted, next_d1, next_wss
                    ));

                    match frame.ack_for.as_deref() {
                        Some("mode") => {
                            if let (Some(realtime), Some(interval_ms), Some(revision)) = (
                                frame.realtime_hint,
                                frame.next_wss_report_after_ms,
                                frame.mode_revision,
                            ) {
                                let interval_ms = interval_ms.clamp(1_000, 300_000);
                                if self
                                    .host
                                    .apply_wss_mode(realtime, interval_ms, revision, false)
                                {
                                    self.set_next_report_after_ms(interval_ms as i64);
                                }
                            } else {
                                self.host.logger().warn(
                                    "WSS mode ACK ignored: realtimeHint, nextWssReportAfterMs, and modeRevision are required",
                                );
                            }
                        }
                        Some("report") => {
                            if let Some(report_id) = frame.report_id.as_deref() {
                                let accepted = frame.accepted.unwrap_or(false);
                                if accepted {
                                    self.reset_server_overload_backoff();
                                }
                                self.host.apply_wss_report_ack(
                                    report_id,
                                    accepted,
                                    frame.persisted,
                                    frame.accepted_samples,
                                );
                            } else {
                                self.host
                                    .logger()
                                    .warn("WSS report ACK ignored: reportId is missing");
                            }
                        }
                        None => {
                            // Compatibility with older servers. Legacy ACKs may still
                            // adjust the interval, but cannot confirm a queued report
                            // because they do not identify it.
                            if frame.persisted == Some(true) {
                                self.host.note_report_success();
                            }
                            if let (Some(realtime), Some(interval_ms)) =
                                (frame.realtime_hint, frame.next_wss_report_after_ms)
                            {
                                let interval_ms = interval_ms.clamp(1_000, 300_000);
                                if self.host.apply_wss_mode(
                                    realtime,
                                    interval_ms,
                                    frame.mode_revision.unwrap_or(0),
                                    false,
                                ) {
                                    self.set_next_report_after_ms(interval_ms as i64);
                                }
                            } else if let Some(interval_ms) = frame.next_wss_report_after_ms {
                                self.set_next_report_after_ms(
                                    interval_ms.clamp(1_000, 300_000) as i64
                                );
                            }
                        }
                        Some(other) => {
                            self.host
                                .logger()
                                .debug(&format!("WSS ACK ignored unknown ackFor={}", other));
                        }
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
                        self.host.logger().warn(&format!(
                            "WSS received schedule inactive frame ts={} reason={}, initiating protocol backoff",
                            frame.ts, schedule_reason
                        ));
                        return Some(WsError::Protocol(format!(
                            "schedule_inactive: {}",
                            schedule_reason
                        )));
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
                    self.host
                        .logger()
                        .debug(&format!("WSS hello repeated ts={}", frame.ts));
                }
                _ => {
                    self.host
                        .logger()
                        .debug(&format!("WSS message ignored type={:?}", frame.r#type));
                }
            }
        }
    }

    fn read_idle_timeout(&self) -> Duration {
        let interval =
            self.report_interval_peek(Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC));
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
        self.host
            .logger()
            .info(&format!("WSS config processed bytes={}", body.len()));
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
        let d = if delay < WSS_FAST_RETRY_INTERVAL {
            WSS_FAST_RETRY_INTERVAL
        } else {
            delay
        };
        self.host.logger().warn(&format!(
            "WSS connection retry backoff triggered: delay={}s reason={}",
            d.as_secs(),
            err_msg
        ));
        self.sleep(d)
    }

    fn handle_connection_error(&self, err: &WsError, backoff: &mut Duration) -> bool {
        if let WsError::Close { code, reason } = err {
            if *code == 1013 {
                let r = format!("WSS close code=1013 (Try Again Later) reason={}", reason);
                let delay = self.next_server_overload_backoff();
                self.host.logger().warn(&format!(
                    "{}, backing off for {}s before reconnect",
                    r,
                    delay.as_secs()
                ));
                if !self.wait_network_retry(&r, delay) {
                    return false;
                }
                *backoff = WSS_FAST_RETRY_INTERVAL;
                return true;
            }
            if *code == 1009 {
                self.host.logger().warn(
                    "WSS peer rejected an oversized report; retaining and shrinking the pending batch",
                );
                self.host.handle_wss_message_too_big();
                if !self.wait_network_retry("WSS close code=1009", WSS_FAST_RETRY_INTERVAL) {
                    return false;
                }
                *backoff = WSS_FAST_RETRY_INTERVAL;
                return true;
            }
            if *code == 1008 {
                self.host.note_report_failure(*code as i32);
                let r = format!(
                    "WSS close code=1008 (Auth/Policy Violation) reason={}",
                    reason
                );
                self.host
                    .logger()
                    .warn(&format!("WSS protocol error: {}", r));
                self.delay_protocol(&r);
                *backoff = WSS_PROTOCOL_RETRY_DELAY;
                return true;
            }
        }
        if let WsError::Protocol(msg) = err {
            if msg.starts_with("server error code=") || msg.starts_with("schedule_inactive:") {
                let delay = if msg.starts_with("schedule_inactive:") {
                    Duration::from_secs(30)
                } else {
                    WSS_PROTOCOL_RETRY_DELAY
                };
                self.host.logger().warn(&format!(
                    "WSS protocol backoff triggered: delay={}s error={}",
                    delay.as_secs(),
                    msg
                ));
                self.delay_protocol(msg);
                *backoff = delay;
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
        *backoff = next_wss_reconnect_backoff(delay);
        true
    }

    fn sleep(&self, d: Duration) -> bool {
        if d.is_zero() {
            return true;
        }
        let inner = self.inner.lock().unwrap();
        let (guard, _timeout_res) = self.wake_cv.wait_timeout(inner, d).unwrap();
        !(self.shutdown.load(AtomicOrdering::Relaxed) || guard.stopped)
    }

    fn next_server_overload_backoff(&self) -> Duration {
        let mut inner = self.inner.lock().unwrap();
        let delay = match inner.server_overload_attempt {
            0 => Duration::from_secs(5),
            1 => Duration::from_secs(10),
            2 => Duration::from_secs(30),
            _ => Duration::from_secs(60),
        };
        inner.server_overload_attempt = inner.server_overload_attempt.saturating_add(1).min(3);
        delay
    }

    fn reset_server_overload_backoff(&self) {
        self.inner.lock().unwrap().server_overload_attempt = 0;
    }
}

impl Clone for ReportTransport {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            inner: self.inner.clone(),
            outbound: self.outbound.clone(),
            socket_wake: self.socket_wake.clone(),
            connected: self.connected.clone(),
            wake_cv: self.wake_cv.clone(),
            shutdown: self.shutdown.clone(),
        }
    }
}
