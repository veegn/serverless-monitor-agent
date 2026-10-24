use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use crate::metrics::{collect_basic_stats, read_cpu_percent, read_net_bytes};
use crate::storage::traffic::calc_monthly_traffic;
use crate::types::{Config, DEFAULT_REPORT_INTERVAL_SEC, DEFAULT_WSS_REPORT_INTERVAL_SEC};

use super::Agent;

pub(super) fn effective_collect_interval(cfg: &Config) -> Duration {
    if cfg.collect_interval == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(cfg.collect_interval)
    }
}

pub(super) fn duration_gcd(mut a: Duration, mut b: Duration) -> Duration {
    while !b.is_zero() {
        let remainder = Duration::from_nanos(a.as_nanos() as u64 % b.as_nanos() as u64);
        a = b;
        b = remainder;
    }
    a
}

impl Agent {
    pub(super) fn discovery_abandoned(&self) -> bool {
        self.discovery_failed.load(AtomicOrdering::Relaxed) && self.config_snapshot().server_id.is_empty()
    }

    pub(super) fn tick_interval(&self) -> Duration {
        let cfg = self.config_snapshot();
        let mut active = self.current_wss_report_interval_for_config(&cfg);
        let collect = effective_collect_interval(&cfg);
        if collect > Duration::ZERO {
            active = duration_gcd(active, collect);
        }
        if active <= Duration::ZERO {
            Duration::from_secs(1)
        } else {
            active
        }
    }

    pub(super) fn current_wss_report_interval_for_config(&self, cfg: &Config) -> Duration {
        let rep = self.reporter.lock().unwrap().clone();
        if let Some(r) = rep {
            if self.uses_wss_config(cfg) {
                let fallback = Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC);
                return r.report_interval(fallback);
            }
        }
        Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC)
    }

    pub(super) fn persistent_uses_wss_config(&self, _cfg: &Config) -> bool {
        true
    }

    pub(super) fn wss_runtime_disabled(&self) -> (bool, String) {
        let disabled_at = self.wss_runtime_disabled_at.lock().unwrap();
        let reason = self.wss_runtime_disabled_reason.lock().unwrap();
        (disabled_at.is_some(), reason.clone())
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

    pub(super) fn report_pause_remaining(&self, now: Instant) -> Duration {
        if let Some(until) = *self.report_paused_until.lock().unwrap() {
            if until > now {
                return until.duration_since(now);
            }
        }
        Duration::ZERO
    }

    pub(super) fn tick(&self) {
        let now = Instant::now();
        if self.discovery_abandoned() {
            self.samples.lock().unwrap().clear();
            return;
        }

        let cfg = self.config_snapshot();
        if self.report_pause_remaining(now) > Duration::ZERO {
            let collect_interval = effective_collect_interval(&cfg);
            let sample_interval = if collect_interval > Duration::ZERO {
                collect_interval
            } else {
                Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC)
            };
            let last_sample = *self.last_sample.lock().unwrap();
            let should_sample =
                last_sample.is_none() || now.duration_since(last_sample.unwrap()) >= sample_interval;

            if should_sample {
                self.record_offline_sample(now, &cfg);
            }
            return;
        }

        let auto_discovery_due = cfg.server_id.is_empty();
        let wss_enabled = self.uses_wss_config(&cfg);
        let rep = self.reporter.lock().unwrap().clone();
        let wss_connected = wss_enabled && rep.as_ref().map(|r| r.connected()).unwrap_or(false);

        let last_report = *self.last_report.lock().unwrap();
        let should_wss_report = wss_connected
            && (last_report.is_none()
                || now.duration_since(last_report.unwrap()) >= self.current_wss_report_interval_for_config(&cfg));

        let collect_interval = effective_collect_interval(&cfg);
        let sample_interval = if collect_interval > Duration::ZERO {
            collect_interval
        } else if !wss_connected && !auto_discovery_due {
            Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC)
        } else {
            Duration::ZERO
        };
        let last_sample = *self.last_sample.lock().unwrap();
        let should_sample = sample_interval > Duration::ZERO
            && (last_sample.is_none() || now.duration_since(last_sample.unwrap()) >= sample_interval);

        if !should_wss_report && !auto_discovery_due && !should_sample {
            return;
        }

        let report_due = should_wss_report || auto_discovery_due;
        let mut report_interval = Duration::from_secs(cfg.report_interval);
        if report_interval < Duration::from_secs(1) {
            report_interval = Duration::from_secs(DEFAULT_REPORT_INTERVAL_SEC);
        }

        let mut full_at_guard = self.full_at.lock().unwrap();
        let full_due = report_due
            && (full_at_guard.is_none()
                || now.duration_since(full_at_guard.unwrap()) >= report_interval);

        if full_due {
            if crate::util::log::maybe_rotate_log_file(&self.paths.log_file, crate::util::log::DEFAULT_MAX_LOG_SIZE) {
                self.log.info(&format!("log file exceeded size limit, rotated to {}.1", self.paths.log_file));
            }
            let stats = collect_basic_stats();
            *self.basic.write().unwrap() = stats;
            *self.basic_at.lock().unwrap() = Some(now);
        } else {
            self.refresh_realtime_basic_stats();
        }

        let (net_now, rx_speed, tx_speed) = {
            let net_res = read_net_bytes(&cfg.interface);
            self.network_rates(now, net_res)
        };

        let cpu = {
            let (usage, ok) = read_cpu_percent();
            if ok {
                format!("{:.2}", usage)
            } else {
                "0.00".to_string()
            }
        };

        if full_due {
            match calc_monthly_traffic(&self.paths.traffic_file, net_now, cfg.reset_day, &cfg.interface) {
                Ok((m_rx, m_tx)) => {
                    *self.monthly_rx.lock().unwrap() = m_rx;
                    *self.monthly_tx.lock().unwrap() = m_tx;
                }
                Err(e) => {
                    self.log.warn_once("traffic-state", &format!("monthly traffic checkpoint failed: {}", e));
                }
            }
            let disk_io = self.sample_disk_io(now);
            *self.disk_io.lock().unwrap() = disk_io;
            *full_at_guard = Some(now);
        }
        drop(full_at_guard);

        let rx_monthly = *self.monthly_rx.lock().unwrap();
        let tx_monthly = *self.monthly_tx.lock().unwrap();
        let disk_io = *self.disk_io.lock().unwrap();

        let m = self.build_metrics(&cfg, &cpu, net_now, rx_speed, tx_speed, rx_monthly, tx_monthly, disk_io);

        if auto_discovery_due {
            let report_at = Instant::now();
            if let Ok((body, sample_count)) = self.build_report_body_for_config(&cfg, &m, report_at) {
                let status_code = self.post_report_body(&cfg, &body, sample_count, false);
                if (200..300).contains(&status_code) {
                    self.persist_pending_server_id();
                }
            }
            return;
        }

        if should_sample {
            if wss_connected {
                self.append_sample(now, &m);
            } else {
                self.record_offline_sample(now, &cfg);
            }
        }

        if should_wss_report {
            let res = self.send_report(&cfg, &m);
            self.apply_report_result(res, now, &cfg, &m);
        }
    }
}
