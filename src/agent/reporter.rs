use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use ring::rand::SecureRandom;
use sha2::{Digest, Sha256};

use crate::storage::cache::{CachedSample, BATCH_REPORT_SIZE};
use crate::transport::TransportHost;
use crate::types::{Config, Metrics, CONFIG_SCHEMA_VERSION, MAX_BUFFERED_METRIC_SAMPLES};

use super::{AdaptiveMode, Agent};

pub const CONFIG_STATE_REPORT_INTERVAL: Duration = Duration::from_secs(60);
pub const ADAPTIVE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
pub const MAX_REPORT_FRAME_BYTES: usize = 512 * 1024;
const REPORT_ACK_TIMEOUT: Duration = Duration::from_secs(15);
const REPORT_RETRY_MAX: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum PendingReportKind {
    InitialSnapshot,
    Realtime,
    MemoryBatch,
    CachedBatch,
}

impl PendingReportKind {
    fn is_batch(self) -> bool {
        matches!(self, Self::MemoryBatch | Self::CachedBatch)
    }
}

#[derive(Debug, Clone)]
pub(super) struct PendingReport {
    pub(super) report_id: String,
    payload: serde_json::Value,
    pub(super) body: Vec<u8>,
    pub(super) kind: PendingReportKind,
    pub(super) sample_count: usize,
    consume_count: usize,
    retry_at: Instant,
    attempts: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedPendingReport {
    kind: PendingReportKind,
    body: String,
    sample_count: usize,
    consume_count: usize,
    attempts: u32,
}

pub(super) fn restore_pending_report(path: &str) -> Option<PendingReport> {
    let saved = fs::read(path).ok()?;
    let persisted: PersistedPendingReport = serde_json::from_slice(&saved).ok()?;
    let body = persisted.body.into_bytes();
    let payload = serde_json::from_slice::<serde_json::Value>(&body).ok()?;
    let report_id = payload.get("reportId")?.as_str()?.to_string();
    let sample_count = body_sample_count(&body);
    if persisted.kind.is_batch() && sample_count != persisted.sample_count {
        return None;
    }
    Some(PendingReport {
        report_id,
        payload,
        body,
        kind: persisted.kind,
        sample_count: persisted.sample_count,
        // Samples held only in the in-memory ring were restored inside this
        // report body; do not remove newly collected samples after restart.
        consume_count: if persisted.kind == PendingReportKind::MemoryBatch {
            0
        } else {
            persisted.consume_count
        },
        retry_at: Instant::now(),
        attempts: persisted.attempts,
    })
}

pub(super) fn jittered_idle_interval(interval: Duration, minimum: Duration) -> Duration {
    let mut random = [0u8; 4];
    let offset = if ring::rand::SystemRandom::new().fill(&mut random).is_ok() {
        (u32::from_be_bytes(random) % 6_001) as i64 - 3_000
    } else {
        0
    };
    let base_ms = interval.as_millis().min(i64::MAX as u128) as i64;
    let minimum_ms = minimum.as_millis().max(1).min(i64::MAX as u128) as i64;
    Duration::from_millis((base_ms + offset).max(minimum_ms) as u64)
}

impl Agent {
    pub(super) fn adaptive_report_due(&self, now: Instant) -> bool {
        let state = self.adaptive_reporting.lock().unwrap();
        state.initial_snapshot_pending
            || state.force_report
            || state.next_report_due.map(|due| now >= due).unwrap_or(false)
    }

    pub(super) fn pending_report_retry_due(&self, now: Instant) -> bool {
        self.pending_report
            .lock()
            .unwrap()
            .as_ref()
            .map(|pending| now >= pending.retry_at)
            .unwrap_or(false)
    }

    pub(super) fn process_adaptive_reporting(&self, cfg: &Config, metrics: &Metrics, now: Instant) {
        if let Some((report_id, body, retry_at)) = {
            let pending = self.pending_report.lock().unwrap();
            pending
                .as_ref()
                .map(|p| (p.report_id.clone(), p.body.clone(), p.retry_at))
        } {
            if now >= retry_at {
                self.send_pending_report(&report_id, &body, now);
            }
            return;
        }

        let (mode, due, initial_snapshot, force, generation) = {
            let state = self.adaptive_reporting.lock().unwrap();
            (
                state.mode,
                state.next_report_due.map(|at| now >= at).unwrap_or(false),
                state.initial_snapshot_pending,
                state.force_report,
                state.generation,
            )
        };
        let cached_count = self.cache.lock().unwrap().count();
        let memory_count = self.samples.lock().unwrap().len();

        let (kind, sample_values, consume_count) = if initial_snapshot {
            (PendingReportKind::InitialSnapshot, Vec::new(), 0)
        } else if cached_count > 0 {
            let batch = self.cache.lock().unwrap().peek_batch(BATCH_REPORT_SIZE);
            if batch.is_empty() {
                return;
            }
            let values = batch.iter().map(cached_sample_value).collect::<Vec<_>>();
            (PendingReportKind::CachedBatch, values, batch.len())
        } else if mode == AdaptiveMode::Realtime && memory_count > 0 {
            let samples = self.memory_sample_values();
            (PendingReportKind::MemoryBatch, samples, memory_count)
        } else if force || due {
            if mode == AdaptiveMode::IdleBatch && memory_count > 0 {
                let samples = self.memory_sample_values();
                (PendingReportKind::MemoryBatch, samples, memory_count)
            } else if mode == AdaptiveMode::IdleBatch {
                (PendingReportKind::InitialSnapshot, Vec::new(), 0)
            } else {
                (PendingReportKind::Realtime, Vec::new(), 0)
            }
        } else {
            return;
        };

        match self.build_pending_report(cfg, metrics, now, kind, sample_values, consume_count) {
            Ok(pending) => {
                let report_id = pending.report_id.clone();
                let body = pending.body.clone();
                *self.pending_report.lock().unwrap() = Some(pending);
                {
                    let mut state = self.adaptive_reporting.lock().unwrap();
                    if state.generation == generation {
                        state.force_report = false;
                    }
                }
                self.send_pending_report(&report_id, &body, now);
            }
            Err(error) => {
                self.log.warn_once(
                    "adaptive-report-build",
                    &format!("failed to build adaptive WSS report: {}", error),
                );
            }
        }
    }

    fn memory_sample_values(&self) -> Vec<serde_json::Value> {
        let samples = self
            .samples
            .lock()
            .unwrap()
            .iter()
            .take(BATCH_REPORT_SIZE)
            .cloned()
            .collect::<Vec<_>>();
        let clock = self.clock.lock().unwrap();
        samples
            .iter()
            .map(|sample| {
                let (ts, _) = clock.timestamp(sample.at);
                serde_json::json!({"ts": ts, "metrics": sample.metrics})
            })
            .collect()
    }

    fn build_pending_report(
        &self,
        cfg: &Config,
        metrics: &Metrics,
        report_at: Instant,
        kind: PendingReportKind,
        mut sample_values: Vec<serde_json::Value>,
        mut consume_count: usize,
    ) -> Result<PendingReport, String> {
        let batch = kind.is_batch();
        if batch && sample_values.is_empty() {
            return Err("refusing to create an empty batch".to_string());
        }
        if sample_values.len() > MAX_BUFFERED_METRIC_SAMPLES {
            sample_values.truncate(MAX_BUFFERED_METRIC_SAMPLES);
            consume_count = consume_count.min(MAX_BUFFERED_METRIC_SAMPLES);
        }

        let time_snapshot = self
            .clock
            .lock()
            .unwrap()
            .snapshot(SystemTime::now(), report_at);
        let metrics_value = corrected_metrics_value(self, metrics, report_at)?;
        let md5 = if cfg.config_md5.is_empty() {
            "none"
        } else {
            &cfg.config_md5
        };
        let mut include_config = self.should_report_config_state(md5, report_at);

        // Keep the earliest samples in this report so trimming the tail preserves FIFO order.
        let (report_id, payload, body) = loop {
            let mut payload = serde_json::json!({
                "id": cfg.server_id,
                "secret": cfg.secret,
                "batch": batch,
                "time": time_snapshot,
                "metrics": metrics_value,
                "collect_interval": cfg.collect_interval,
                "report_interval": cfg.report_interval,
            });
            let map = payload.as_object_mut().unwrap();
            if batch {
                map.insert(
                    "samples".to_string(),
                    serde_json::Value::Array(sample_values.clone()),
                );
            }
            if !cfg.config_md5.is_empty() && include_config {
                map.insert(
                    "config_schema".to_string(),
                    serde_json::Value::String(CONFIG_SCHEMA_VERSION.to_string()),
                );
                map.insert(
                    "config_md5".to_string(),
                    serde_json::Value::String(md5.to_string()),
                );
            }
            let report_id = if batch {
                batch_report_id(&payload)?
            } else {
                random_report_id()?
            };
            payload["reportId"] = serde_json::Value::String(report_id.clone());
            let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
            if body.len() <= MAX_REPORT_FRAME_BYTES {
                break (report_id, payload, body);
            }
            if batch && sample_values.len() > 1 {
                sample_values.pop();
                consume_count = consume_count.min(sample_values.len());
                continue;
            }
            if include_config {
                include_config = false;
                continue;
            }
            return Err(format!(
                "single report frame is {} bytes, exceeding {} byte limit",
                body.len(),
                MAX_REPORT_FRAME_BYTES
            ));
        };

        let sample_count = if batch { sample_values.len() } else { 0 };
        Ok(PendingReport {
            report_id,
            payload,
            body,
            kind,
            sample_count,
            consume_count: if batch {
                consume_count.min(sample_count)
            } else {
                0
            },
            retry_at: report_at,
            attempts: 0,
        })
    }

    fn send_pending_report(&self, report_id: &str, body: &[u8], now: Instant) {
        let pending = self
            .pending_report
            .lock()
            .unwrap()
            .as_ref()
            .filter(|pending| pending.report_id == report_id)
            .cloned();
        let Some(pending) = pending else { return };
        if let Err(error) = self.persist_pending_report(&pending) {
            self.log.warn_once(
                "adaptive-pending-save",
                &format!(
                    "failed to persist pending WSS report; report was not sent: {}",
                    error
                ),
            );
            if let Some(pending) = self
                .pending_report
                .lock()
                .unwrap()
                .as_mut()
                .filter(|pending| pending.report_id == report_id)
            {
                pending.retry_at = now + Duration::from_secs(2);
            }
            return;
        }

        let reporter = self.reporter.lock().unwrap().clone();
        let sent = reporter
            .as_ref()
            .filter(|transport| transport.connected())
            .map(|transport| transport.send(body))
            .unwrap_or(false);

        let mut pending_guard = self.pending_report.lock().unwrap();
        if let Some(pending) = pending_guard.as_mut().filter(|p| p.report_id == report_id) {
            pending.attempts = pending.attempts.saturating_add(1);
            pending.retry_at = if sent {
                now + REPORT_ACK_TIMEOUT
            } else {
                now + Duration::from_secs(2)
            };
        }
        if sent {
            self.log.debug(&format!(
                "WSS report sent reportId={} payload_bytes={} batch={} samples={}",
                report_id,
                body.len(),
                body_is_batch(body),
                body_sample_count(body)
            ));
        }
    }

    fn persist_pending_report(&self, pending: &PendingReport) -> Result<(), String> {
        let parent = Path::new(&self.pending_report_path)
            .parent()
            .unwrap_or(Path::new(""));
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let body = String::from_utf8(pending.body.clone()).map_err(|error| error.to_string())?;
        let persisted = PersistedPendingReport {
            kind: pending.kind,
            body,
            sample_count: pending.sample_count,
            consume_count: pending.consume_count,
            attempts: pending.attempts,
        };
        let encoded = serde_json::to_vec(&persisted).map_err(|error| error.to_string())?;
        let temp_path = format!("{}.tmp", self.pending_report_path);
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|error| error.to_string())?;
        file.write_all(&encoded)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        if cfg!(windows) && Path::new(&self.pending_report_path).exists() {
            fs::remove_file(&self.pending_report_path).map_err(|error| error.to_string())?;
        }
        fs::rename(&temp_path, &self.pending_report_path).map_err(|error| error.to_string())
    }

    fn clear_persisted_pending_report(&self) {
        if let Err(error) = fs::remove_file(&self.pending_report_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                self.log.warn_once(
                    "adaptive-pending-remove",
                    &format!("failed to clear confirmed WSS report marker: {}", error),
                );
            }
        }
    }

    pub(super) fn handle_wss_report_ack(
        &self,
        report_id: &str,
        accepted: bool,
        persisted: Option<bool>,
        accepted_samples: Option<usize>,
    ) {
        let mut pending = {
            let mut guard = self.pending_report.lock().unwrap();
            if guard.as_ref().map(|p| p.report_id.as_str()) != Some(report_id) {
                self.log.debug(&format!(
                    "ignored unmatched WSS report ACK reportId={}",
                    report_id
                ));
                return;
            }
            guard.take().unwrap()
        };

        let batch_confirmed = !pending.kind.is_batch()
            || (persisted == Some(true) && accepted_samples == Some(pending.sample_count));
        if !accepted || !batch_confirmed {
            pending.retry_at = Instant::now() + report_retry_delay(pending.attempts);
            self.log.warn(&format!(
                "WSS report not committed reportId={} accepted={} persisted={:?} acceptedSamples={:?}; retaining for retry",
                report_id, accepted, persisted, accepted_samples
            ));
            *self.pending_report.lock().unwrap() = Some(pending);
            self.wake_tick();
            return;
        }

        self.note_report_success();

        let remove_result = match pending.kind {
            PendingReportKind::MemoryBatch => {
                let mut samples = self.samples.lock().unwrap();
                let remove = pending.consume_count.min(samples.len());
                samples.drain(0..remove);
                Ok(remove)
            }
            PendingReportKind::CachedBatch => {
                let cache = self.cache.lock().unwrap();
                let queued = cache.peek_batch(pending.consume_count);
                let expected = pending
                    .payload
                    .get("samples")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                let first_sent = pending.consume_count.saturating_sub(expected.len());
                let queued_sent = queued
                    .iter()
                    .skip(first_sent)
                    .map(cached_sample_value)
                    .collect::<Vec<_>>();
                if queued_sent == expected {
                    cache.drain(pending.consume_count)
                } else {
                    // The sidecar can outlive a successful drain if the process
                    // stopped before it removed the marker.
                    Ok(0)
                }
            }
            PendingReportKind::InitialSnapshot | PendingReportKind::Realtime => Ok(0),
        };
        if let Err(error) = remove_result {
            self.log.warn(&format!(
                "WSS report persisted but local queue cleanup failed reportId={} error={}; retrying idempotently",
                report_id, error
            ));
            pending.attempts = pending.attempts.saturating_add(1);
            pending.retry_at = Instant::now() + Duration::from_secs(2);
            *self.pending_report.lock().unwrap() = Some(pending);
            self.wake_tick();
            return;
        }
        self.clear_persisted_pending_report();

        self.persist_pending_server_id();
        *self.last_report.lock().unwrap() = Some(Instant::now());
        if self
            .discovery_just_bound
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.samples.lock().unwrap().clear();
            self.discovery_sync_pending
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let now = Instant::now();
        {
            let mut state = self.adaptive_reporting.lock().unwrap();
            if pending.kind == PendingReportKind::InitialSnapshot {
                state.initial_snapshot_pending = false;
            }
            if state.force_report {
                state.next_report_due = Some(now);
            } else if state.mode == AdaptiveMode::IdleBatch {
                state.next_report_due =
                    Some(now + jittered_idle_interval(state.interval, ADAPTIVE_SAMPLE_INTERVAL));
            } else {
                state.next_report_due = Some(now + state.interval);
            }
        }

        if self
            .discovery_sync_pending
            .swap(false, std::sync::atomic::Ordering::Relaxed)
            && self.uses_wss()
        {
            self.sync_report_transport();
        }
        self.wake_tick();
    }

    pub(super) fn shrink_pending_report(&self) {
        let mut guard = self.pending_report.lock().unwrap();
        let Some(pending) = guard.as_mut() else {
            return;
        };
        if !pending.kind.is_batch() {
            self.log.warn(
                "server rejected a non-batch WSS report as too large; retaining it for retry",
            );
            pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
            return;
        }

        let Some(samples) = pending
            .payload
            .get("samples")
            .and_then(|v| v.as_array())
            .cloned()
        else {
            return;
        };
        let current_len = samples.len();
        if current_len <= 1 {
            self.log.error_once(
                "wss-report-single-sample-too-large",
                "server rejected a single-sample WSS batch as too large; retained it and stopped rapid retries",
            );
            pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
            return;
        }
        let mut keep = if current_len > 30 {
            30
        } else {
            (current_len / 2).max(1)
        };
        loop {
            let first_kept = current_len.saturating_sub(keep);
            let resized = samples[first_kept..].to_vec();
            pending.payload["samples"] = serde_json::Value::Array(resized);
            let new_id = match batch_report_id(&pending.payload) {
                Ok(id) => id,
                Err(error) => {
                    self.log
                        .warn(&format!("could not create resized report ID: {}", error));
                    pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
                    return;
                }
            };
            pending.payload["reportId"] = serde_json::Value::String(new_id.clone());
            match serde_json::to_vec(&pending.payload) {
                Ok(body) if body.len() <= MAX_REPORT_FRAME_BYTES => {
                    pending.sample_count = keep;
                    pending.report_id = new_id;
                    pending.body = body;
                    pending.retry_at = Instant::now();
                    self.log.warn(&format!(
                        "resized oversized WSS batch from {} to {} samples; dropped oldest samples and will retry",
                        current_len, keep
                    ));
                    self.wake_tick();
                    return;
                }
                Ok(_) if keep > 1 => keep = (keep / 2).max(1),
                Ok(_) => {
                    self.log.error_once(
                        "wss-report-single-sample-too-large",
                        "a single-sample WSS batch still exceeds the frame limit; retained for delayed retry",
                    );
                    pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
                    return;
                }
                Err(error) => {
                    self.log.warn(&format!(
                        "could not serialize resized WSS report: {}",
                        error
                    ));
                    pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
                    return;
                }
            }
        }
    }

    pub(super) fn build_report_body_for_config(
        &self,
        cfg: &Config,
        metrics: &Metrics,
        report_at: Instant,
    ) -> Result<(Vec<u8>, usize), String> {
        let time_snapshot = self
            .clock
            .lock()
            .unwrap()
            .snapshot(SystemTime::now(), report_at);
        let metrics_value = corrected_metrics_value(self, metrics, report_at)?;
        let mut payload = serde_json::json!({
            "id": cfg.server_id,
            "secret": cfg.secret,
            "time": time_snapshot,
            "metrics": metrics_value,
            "collect_interval": cfg.collect_interval,
            "report_interval": cfg.report_interval,
        });

        if cfg.server_id.is_empty() {
            let server_name = if !cfg.name.trim().is_empty() {
                cfg.name.trim().to_string()
            } else {
                crate::util::command::get_system_hostname()
            };
            payload
                .as_object_mut()
                .unwrap()
                .insert("name".to_string(), serde_json::Value::String(server_name));
        }

        let md5 = if cfg.config_md5.is_empty() {
            "none"
        } else {
            &cfg.config_md5
        };
        if self.should_report_config_state(md5, report_at) {
            let map = payload.as_object_mut().unwrap();
            map.insert(
                "config_schema".to_string(),
                serde_json::Value::String(CONFIG_SCHEMA_VERSION.to_string()),
            );
            map.insert(
                "config_md5".to_string(),
                serde_json::Value::String(md5.to_string()),
            );
        }

        let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        Ok((body, 0))
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
}

fn corrected_metrics_value(
    agent: &Agent,
    metrics: &Metrics,
    report_at: Instant,
) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(metrics).map_err(|e| e.to_string())?;
    if let Ok(boot_time) = metrics.boot_time.parse::<i64>() {
        if boot_time > 0 {
            let corrected = agent.clock.lock().unwrap().correct_local_timestamp(
                boot_time,
                report_at,
                SystemTime::now(),
            );
            if let Some(map) = value.as_object_mut() {
                map.insert(
                    "boot_time".to_string(),
                    serde_json::Value::String(corrected.to_string()),
                );
            }
        }
    }
    Ok(value)
}

fn cached_sample_value(sample: &CachedSample) -> serde_json::Value {
    serde_json::json!({ "ts": sample.ts, "metrics": sample.metrics })
}

fn batch_report_id(payload: &serde_json::Value) -> Result<String, String> {
    let mut material_payload = payload.clone();
    if let Some(object) = material_payload.as_object_mut() {
        object.remove("reportId");
    }
    let material = serde_json::to_vec(&material_payload).map_err(|e| e.to_string())?;
    let digest = Sha256::digest(&material);
    Ok(format!("b-{}", hex::encode(&digest[..16])))
}

fn random_report_id() -> Result<String, String> {
    let mut random = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "secure random generator failed".to_string())?;
    Ok(format!("r-{}", hex::encode(random)))
}

fn report_retry_delay(attempts: u32) -> Duration {
    let secs = match attempts {
        0 | 1 => 5,
        2 => 10,
        3 => 30,
        _ => REPORT_RETRY_MAX.as_secs(),
    };
    Duration::from_secs(secs.min(REPORT_RETRY_MAX.as_secs()))
}

fn body_is_batch(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("batch").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

fn body_sample_count(body: &[u8]) -> usize {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("samples").and_then(|s| s.as_array()).map(Vec::len))
        .unwrap_or(0)
}
