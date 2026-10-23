use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant, SystemTime};

use crate::metrics::sample_metrics_to_map;
use crate::storage::cache::{CachedSample, BATCH_REPORT_SIZE};
use crate::types::{Config, Metrics, CONFIG_SCHEMA_VERSION};

use super::Agent;

pub const CONFIG_STATE_REPORT_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ReportSendResult {
    pub ok: bool,
}

impl Agent {
    pub(super) fn send_report(
        &self,
        cfg: &Config,
        m: &Metrics,
    ) -> ReportSendResult {
        let report_at = Instant::now();
        let (body, sample_count) = match self.build_report_body_for_config(cfg, m, report_at) {
            Ok(res) => res,
            Err(e) => {
                self.log.warn_once("marshal", &format!("marshal payload failed: {}", e));
                return ReportSendResult::default();
            }
        };

        let rep = self.reporter.lock().unwrap().clone();
        if let Some(r) = rep {
            self.log.debug(&format!(
                "WSS report attempt url={} payload_bytes={} samples={}",
                r.url(),
                body.len(),
                sample_count
            ));
            if r.send(&body) {
                return ReportSendResult {
                    ok: true,
                };
            }
        }

        ReportSendResult::default()
    }

    pub(super) fn build_report_body_for_config(
        &self,
        cfg: &Config,
        m: &Metrics,
        report_at: Instant,
    ) -> Result<(Vec<u8>, usize), String> {
        let time_snapshot = self.clock.lock().unwrap().snapshot(SystemTime::now(), report_at);
        let mut metrics_val = serde_json::to_value(m).map_err(|e| e.to_string())?;

        // Correct boot time if needed
        if let Ok(boot_time) = m.boot_time.parse::<i64>() {
            if boot_time > 0 {
                let corrected = self.clock.lock().unwrap().correct_local_timestamp(boot_time, report_at, SystemTime::now());
                if let Some(map) = metrics_val.as_object_mut() {
                    map.insert("boot_time".to_string(), serde_json::Value::String(corrected.to_string()));
                }
            }
        }

        let mut payload = serde_json::json!({
            "id": cfg.server_id,
            "secret": cfg.secret,
            "time": time_snapshot,
            "metrics": metrics_val,
            "collect_interval": cfg.collect_interval,
            "report_interval": cfg.report_interval,
        });

        let map = payload.as_object_mut().unwrap();

        if cfg.server_id.is_empty() {
            let server_name = if !cfg.name.trim().is_empty() {
                cfg.name.trim().to_string()
            } else {
                crate::util::command::get_system_hostname()
            };
            map.insert("name".to_string(), serde_json::Value::String(server_name));
        }

        let md5 = if cfg.config_md5.is_empty() { "none" } else { &cfg.config_md5 };
        if self.should_report_config_state(md5, report_at) {
            map.insert("config_schema".to_string(), serde_json::Value::String(CONFIG_SCHEMA_VERSION.to_string()));
            map.insert("config_md5".to_string(), serde_json::Value::String(md5.to_string()));
        }

        let samples_guard = self.samples.lock().unwrap();
        let sample_count = samples_guard.len();
        if sample_count > 0 {
            let mut samples_arr = Vec::with_capacity(sample_count);
            let clk = self.clock.lock().unwrap();
            for s in samples_guard.iter() {
                let (ts, _) = clk.timestamp(s.at);
                samples_arr.push(serde_json::json!({
                    "ts": ts,
                    "metrics": s.metrics,
                }));
            }
            map.insert("samples".to_string(), serde_json::Value::Array(samples_arr));
        }

        let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        Ok((body, sample_count))
    }

    pub(super) fn should_report_config_state(&self, config_md5: &str, report_at: Instant) -> bool {
        let mut last_at = self.last_config_state_report_at.lock().unwrap();
        let mut last_md5 = self.last_config_state_report_md5.lock().unwrap();

        if last_at.is_none()
            || *last_md5 != config_md5
            || report_at.duration_since(last_at.unwrap()) >= CONFIG_STATE_REPORT_INTERVAL
        {
            *last_at = Some(report_at);
            *last_md5 = config_md5.to_string();
            true
        } else {
            false
        }
    }

    pub(super) fn apply_report_result(&self, result: ReportSendResult, now: Instant, cfg: &Config, m: &Metrics) {
        self.persist_pending_server_id();
        if self.discovery_just_bound.swap(false, AtomicOrdering::Relaxed) {
            self.samples.lock().unwrap().clear();
            self.discovery_sync_pending.store(true, AtomicOrdering::Relaxed);
            return;
        }

        if result.ok {
            *self.last_report.lock().unwrap() = Some(now);
            self.samples.lock().unwrap().clear();
            self.drain_cached_samples(cfg);
        } else {
            // Communication failure: save in-memory samples and current metrics to local disk cache
            let cache_guard = self.cache.lock().unwrap();
            let clk = self.clock.lock().unwrap();
            let mut to_cache = Vec::new();
            let mut mem_samples = self.samples.lock().unwrap();
            for s in mem_samples.drain(..) {
                let (ts, _) = clk.timestamp(s.at);
                to_cache.push(CachedSample {
                    ts,
                    metrics: s.metrics,
                });
            }
            let (current_ts, _) = clk.timestamp(now);
            to_cache.push(CachedSample {
                ts: current_ts,
                metrics: sample_metrics_to_map(m),
            });
            let added = cache_guard.append_samples(&to_cache).unwrap_or(0);
            let total = cache_guard.count();
            drop(cache_guard);
            drop(mem_samples);

            self.log.warn_once("report-cache-offline", &format!(
                "server communication failure, cached {} samples to disk (total cached: {})",
                added, total
            ));
        }

        if self.discovery_sync_pending.swap(false, AtomicOrdering::Relaxed) && self.uses_wss() {
            self.sync_report_transport();
        }
    }

    pub(super) fn drain_cached_samples(&self, cfg: &Config) {
        if cfg.server_id.is_empty() {
            return;
        }
        let initial_count = self.cache.lock().unwrap().count();
        if initial_count == 0 {
            return;
        }

        self.log.info(&format!(
            "server communication restored, batch reporting cached samples (pending: {})...",
            initial_count
        ));

        let report_at = Instant::now();
        let time_snapshot = self.clock.lock().unwrap().snapshot(SystemTime::now(), report_at);

        while self.cache.lock().unwrap().count() > 0 {
            let batch = self.cache.lock().unwrap().peek_batch(BATCH_REPORT_SIZE);
            if batch.is_empty() {
                break;
            }
            let batch_len = batch.len();

            let samples_val: Vec<serde_json::Value> = batch
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "ts": s.ts,
                        "metrics": s.metrics,
                    })
                })
                .collect();

            let payload = serde_json::json!({
                "id": cfg.server_id,
                "secret": cfg.secret,
                "time": time_snapshot,
                "samples": samples_val,
                "batch": true,
            });

            let body = match serde_json::to_vec(&payload) {
                Ok(b) => b,
                Err(e) => {
                    self.log.warn(&format!("failed to serialize batch report payload: {}", e));
                    break;
                }
            };

            let mut sent = false;
            let rep = self.reporter.lock().unwrap().clone();
            if self.uses_wss_config(cfg) {
                if let Some(r) = rep.as_ref() {
                    if r.connected() {
                        sent = r.send(&body);
                    }
                }
            }

            if sent {
                let _ = self.cache.lock().unwrap().drain(batch_len);
                let remaining = self.cache.lock().unwrap().count();
                self.log.info(&format!(
                    "batch reported {} cached samples via WSS, remaining: {}",
                    batch_len, remaining
                ));
                if remaining == 0 {
                    self.log.info("all cached samples reported successfully");
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            } else {
                self.log.warn("batch report delivery failed, will retry on next cycle");
                break;
            }
        }
    }
}
