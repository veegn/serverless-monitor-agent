use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use crate::types::Config;

use super::Agent;

impl Agent {
    pub(super) fn discovery_abandoned(&self) -> bool {
        self.discovery_failed.load(AtomicOrdering::Relaxed)
            && self.config_snapshot().server_id.is_empty()
    }

    pub(super) fn tick_interval(&self) -> Duration {
        let now = Instant::now();
        let mut next = *self.next_sample_due.lock().unwrap();
        let transport = self.reporter.lock().unwrap().clone();
        if transport
            .as_ref()
            .is_some_and(|transport| transport.connected())
        {
            if let Some(report_due) = self.next_reporting_deadline() {
                next = next.min(report_due);
            }
        }
        next.saturating_duration_since(now)
    }

    pub(super) fn persistent_uses_wss_config(&self, _cfg: &Config) -> bool {
        true
    }

    pub(super) fn wss_runtime_disabled(&self) -> (bool, String) {
        let mut disabled_at = self.wss_runtime_disabled_at.lock().unwrap();
        let reason = self.wss_runtime_disabled_reason.lock().unwrap();
        if let Some(at) = *disabled_at {
            if at.elapsed() >= Duration::from_secs(60) {
                *disabled_at = None;
                return (false, String::new());
            }
            return (true, reason.clone());
        }
        (false, String::new())
    }

    pub(super) fn uses_wss(&self) -> bool {
        self.uses_wss_config(&self.config_snapshot())
    }

    pub(super) fn uses_wss_config(&self, cfg: &Config) -> bool {
        if cfg.server_id.is_empty() {
            return false;
        }
        if !self.persistent_uses_wss_config(cfg) {
            return false;
        }
        let (disabled, _) = self.wss_runtime_disabled();
        !disabled
    }

    pub(super) fn tick(&self) {
        if self.discovery_abandoned() {
            return;
        }
        let now = Instant::now();
        let cfg = self.config_snapshot();
        let discovery_due = cfg.server_id.is_empty();
        if !discovery_due && self.uses_wss_config(&cfg) {
            // Also recover a transport stopped during a temporary schedule gap.
            self.sync_report_transport();
        }
        let connected = self
            .reporter
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|transport| transport.connected());
        let sample_due = now >= *self.next_sample_due.lock().unwrap();
        let report_due = connected && self.next_reporting_deadline().is_some_and(|at| now >= at);
        if !sample_due && !report_due {
            return;
        }

        {
            let mut maintenance = self.full_at.lock().unwrap();
            if maintenance
                .map(|last| now.duration_since(last) >= Duration::from_secs(60))
                .unwrap_or(true)
            {
                if crate::util::log::maybe_rotate_log_file(
                    &self.paths.log_file,
                    crate::util::log::DEFAULT_MAX_LOG_SIZE,
                ) {
                    self.log
                        .info("agent log rotated after reaching the configured size limit");
                }
                *maintenance = Some(now);
            }
        }
        let metrics = self.collect_current_metrics(now, &cfg);
        if sample_due {
            if !discovery_due {
                self.append_sample(now, &metrics);
            }
            let finished = Instant::now();
            let mut next = self.next_sample_due.lock().unwrap();
            *next = advance_deadline(*next, super::reporter::ADAPTIVE_SAMPLE_INTERVAL, finished);
        }
        if discovery_due {
            if let Ok((body, sample_count)) = self.build_report_body_for_config(&cfg, &metrics, now)
            {
                let status = self.post_report_body(&cfg, &body, sample_count, false);
                if (200..300).contains(&status) {
                    self.persist_pending_server_id();
                    if !self.config_snapshot().server_id.is_empty() {
                        self.sync_report_transport();
                    }
                }
            }
            return;
        }
        if connected {
            self.process_adaptive_reporting(&cfg, &metrics, now);
        }
    }
}

fn advance_deadline(deadline: Instant, interval: Duration, now: Instant) -> Instant {
    let elapsed = now.saturating_duration_since(deadline);
    let periods = elapsed.as_nanos() / interval.as_nanos() + 1;
    let delay = interval.as_nanos() * periods;
    deadline + Duration::from_nanos(delay.min(u64::MAX as u128) as u64)
}
