//! WSS schedule and connection backoff rules.

use std::collections::HashMap;
use std::time::Duration;

use super::frames::WsServerFrame;

pub const WSS_PROTOCOL_RETRY_DELAY: Duration = Duration::from_secs(120);
pub const WSS_FAST_RETRY_INTERVAL: Duration = Duration::from_secs(2);
pub const WSS_NETWORK_MIN_RETRY: Duration = Duration::from_secs(60);
pub const WSS_NETWORK_MAX_RETRY: Duration = Duration::from_secs(300);

pub const AGENT_WSS_MODE_HEADER: &str = "x-agent-wss-mode";
pub const AGENT_WSS_REASON_HEADER: &str = "x-agent-wss-reason";
pub const AGENT_WSS_MODE_ACTIVE: &str = "active";
pub const AGENT_WSS_MODE_INACTIVE: &str = "inactive";
pub const AGENT_WSS_MODE_DISABLED: &str = "disabled";
pub const AGENT_WSS_SCHEDULE_INACTIVE: &str = "wss_schedule_inactive";
pub const AGENT_WSS_SCHEDULE_EMPTY: &str = "wss_schedule_empty";
pub const AGENT_WSS_SCHEDULE_DISABLED: &str = "wss_disabled";

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
