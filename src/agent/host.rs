use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use crate::transport::{
    is_wss_schedule_inactive_reason, TransportHost, AGENT_WSS_MODE_ACTIVE,
    AGENT_WSS_MODE_DISABLED, AGENT_WSS_MODE_HEADER, AGENT_WSS_MODE_INACTIVE,
    AGENT_WSS_REASON_HEADER, AGENT_WSS_SCHEDULE_INACTIVE,
};
use crate::types::Config;
use crate::util::log::Logger;

use super::Agent;

pub const REPORT_FAILURE_THRESHOLD: usize = 3;
pub const REPORT_BACKOFF_MAX: Duration = Duration::from_secs(600);
pub const REPORT_FAILURE_DECAY: Duration = Duration::from_secs(1800);

const REPORT_BACKOFF_STEPS: [Duration; 5] = [
    Duration::from_secs(120),
    Duration::from_secs(180),
    Duration::from_secs(300),
    Duration::from_secs(420),
    REPORT_BACKOFF_MAX,
];

pub(super) fn report_backoff_for_failures(failures: usize) -> Duration {
    if failures < REPORT_FAILURE_THRESHOLD {
        return Duration::ZERO;
    }
    let index = failures - REPORT_FAILURE_THRESHOLD;
    if index >= REPORT_BACKOFF_STEPS.len() {
        REPORT_BACKOFF_STEPS[REPORT_BACKOFF_STEPS.len() - 1]
    } else {
        REPORT_BACKOFF_STEPS[index]
    }
}

impl TransportHost for Agent {
    fn logger(&self) -> Logger {
        self.log.clone()
    }

    fn config_snapshot(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    fn uses_wss(&self) -> bool {
        self.uses_wss_config(&self.config_snapshot())
    }

    fn calibrate_date(&self, date_str: &str, rtt: Duration) {
        if let Some(date_time) = crate::util::clock::parse_http_date(date_str) {
            let clk = self.clock.lock().unwrap();
            let (snapshot, updated) = clk.update_date(date_time, rtt, SystemTime::now(), Instant::now());
            if updated {
                self.log.debug(&format!(
                    "Date header time calibrated offset_ms={} round_trip_ms={}",
                    snapshot.offset_ms.unwrap_or(0),
                    snapshot.round_trip_ms.unwrap_or(0)
                ));
            } else {
                self.log.debug(&format!(
                    "Date header time calibration skipped offset_ms={} threshold_ms={}",
                    snapshot.offset_ms.unwrap_or(0),
                    20_000
                ));
            }
        }
    }

    fn note_report_failure(&self, status_code: i32) {
        let mut failures_guard = self.report_failures.lock().unwrap();
        let mut last_failure = self.last_report_failure_at.lock().unwrap();
        let mut paused_until = self.report_paused_until.lock().unwrap();
        let mut backoff_guard = self.report_backoff.lock().unwrap();

        let now = Instant::now();
        if let Some(last) = *last_failure {
            if now.duration_since(last) >= REPORT_FAILURE_DECAY {
                *failures_guard = 0;
                *backoff_guard = Duration::ZERO;
            }
        }
        *last_failure = Some(now);
        *failures_guard += 1;
        let failures = *failures_guard;
        let previous_backoff = *backoff_guard;
        let mut delay = Duration::ZERO;

        if failures >= REPORT_FAILURE_THRESHOLD {
            let new_backoff = report_backoff_for_failures(failures);
            *backoff_guard = new_backoff;
            *paused_until = Some(now + new_backoff);
            delay = new_backoff;
        }

        if delay > Duration::ZERO && delay != previous_backoff {
            self.log.warn(&format!(
                "report paused due to failure circuit breaker failures={} last_status={} delay={}s",
                failures,
                status_code,
                delay.as_secs()
            ));
        }
    }

    fn note_report_success(&self) {
        let mut failures = self.report_failures.lock().unwrap();
        let mut paused_until = self.report_paused_until.lock().unwrap();
        let mut backoff = self.report_backoff.lock().unwrap();
        let mut last_failure = self.last_report_failure_at.lock().unwrap();

        *failures = 0;
        *paused_until = None;
        *backoff = Duration::ZERO;
        *last_failure = None;
    }

    fn disable_wss_runtime(&self, reason: &str) {
        let r = if reason.trim().is_empty() {
            AGENT_WSS_SCHEDULE_INACTIVE
        } else {
            reason.trim()
        };
        let mut disabled_at = self.wss_runtime_disabled_at.lock().unwrap();
        let mut disabled_reason = self.wss_runtime_disabled_reason.lock().unwrap();

        let already_disabled = disabled_at.is_some() && *disabled_reason == r;
        if already_disabled {
            return;
        }
        *disabled_at = Some(Instant::now());
        *disabled_reason = r.to_string();
        drop(disabled_at);
        drop(disabled_reason);

        self.log.info(&format!(
            "WSS temporarily disabled reason={}; using POST report",
            r
        ));
        let reporter = self.reporter.lock().unwrap().clone();
        if let Some(rep) = reporter {
            rep.stop(r);
        }
        self.wake_tick();
    }

    fn apply_wss_remote_config(&self, body: &[u8], headers: &HashMap<String, String>) -> Result<(), String> {
        self.apply_remote_config_with_options(body, headers, true)
    }

    fn wake_tick(&self) {
        let mut flag = self.wake_mutex.lock().unwrap();
        *flag = true;
        self.wake_cv.notify_all();
    }
}

impl Agent {
    pub(super) fn handle_wss_runtime_headers(&self, headers: &HashMap<String, String>) {
        let mode = headers
            .get(AGENT_WSS_MODE_HEADER)
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let reason = headers
            .get(AGENT_WSS_REASON_HEADER)
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();

        match mode.as_str() {
            AGENT_WSS_MODE_ACTIVE => {
                self.clear_wss_runtime_disabled(&reason);
            }
            AGENT_WSS_MODE_INACTIVE | AGENT_WSS_MODE_DISABLED => {
                if is_wss_schedule_inactive_reason(&reason) {
                    self.disable_wss_runtime(&reason);
                }
            }
            _ => {}
        }
    }

    pub(super) fn clear_wss_runtime_disabled(&self, reason: &str) {
        let mut disabled_at = self.wss_runtime_disabled_at.lock().unwrap();
        let mut disabled_reason = self.wss_runtime_disabled_reason.lock().unwrap();

        let was_disabled = disabled_at.is_some();
        *disabled_at = None;
        disabled_reason.clear();
        drop(disabled_at);
        drop(disabled_reason);

        if !was_disabled {
            return;
        }
        let r = if reason.is_empty() { "server_active" } else { reason };
        self.log.info(&format!("WSS temporary disable cleared reason={}", r));
        self.sync_report_transport();
        self.wake_tick();
    }

    pub(super) fn sync_report_transport(&self) {
        if self.uses_wss() {
            let reporter = self.reporter.lock().unwrap().clone();
            if let Some(r) = reporter {
                r.start();
            }
            return;
        }
        let reporter = self.reporter.lock().unwrap().clone();
        if let Some(r) = reporter {
            r.stop("connection_mode=http");
        }
    }
}
