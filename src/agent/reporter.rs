use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use ring::rand::SecureRandom;
use sha2::{Digest, Sha256};

use super::{AdaptiveMode, Agent};
use crate::storage::cache::{CachedSample, BATCH_REPORT_SIZE};
use crate::transport::TransportHost;
use crate::types::{Config, Metrics, CONFIG_SCHEMA_VERSION, MAX_BUFFERED_METRIC_SAMPLES};

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
    body: Vec<u8>,
    kind: PendingReportKind,
    sample_count: usize,
    consume_count: usize,
    retry_at: Instant,
    attempts: u32,
    persisted: bool,
    generation: u64,
    source_samples: Vec<CachedSample>,
}

pub(super) struct ReportingState {
    history: Option<PendingReport>,
    latest: Option<PendingReport>,
    pub(super) next_history_due: Instant,
    build_retry_at: Instant,
}

impl Default for ReportingState {
    fn default() -> Self {
        Self {
            history: None,
            latest: None,
            next_history_due: Instant::now() + Duration::from_secs(60),
            build_retry_at: Instant::now(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedPendingReport {
    kind: PendingReportKind,
    body: String,
    sample_count: usize,
    consume_count: usize,
    attempts: u32,
    #[serde(default)]
    source_samples: Vec<CachedSample>,
}

pub(super) fn restore_reporting_state(path: &str) -> Result<ReportingState, String> {
    let saved = match fs::read(path) {
        Ok(saved) => saved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReportingState::default())
        }
        Err(error) => return Err(error.to_string()),
    };
    let persisted: PersistedPendingReport =
        serde_json::from_slice(&saved).map_err(|e| e.to_string())?;
    let body = persisted.body.into_bytes();
    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let report_id = payload
        .get("reportId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or("pending report has no reportId")?
        .to_string();
    if body.len() > MAX_REPORT_FRAME_BYTES || body_sample_count(&body) != persisted.sample_count {
        return Err("pending report size or sample count is invalid".to_string());
    }
    let source_samples = if persisted.source_samples.is_empty()
        && persisted.kind == PendingReportKind::CachedBatch
    {
        payload
            .get("samples")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|sample| serde_json::from_value(sample.clone()).ok())
            .collect()
    } else {
        persisted.source_samples
    };
    let pending = PendingReport {
        report_id,
        payload,
        body,
        kind: persisted.kind,
        sample_count: persisted.sample_count,
        consume_count: persisted.consume_count,
        retry_at: Instant::now(),
        attempts: persisted.attempts,
        persisted: true,
        generation: 0,
        source_samples,
    };
    let mut reporting = ReportingState::default();
    if pending.kind.is_batch() {
        reporting.history = Some(pending);
    } else {
        reporting.latest = Some(pending);
    }
    Ok(reporting)
}

pub(super) fn jittered_idle_interval(interval: Duration, minimum: Duration) -> Duration {
    let mut random = [0u8; 4];
    let offset = if ring::rand::SystemRandom::new().fill(&mut random).is_ok() {
        (u32::from_be_bytes(random) % 6_001) as i64 - 3_000
    } else {
        0
    };
    Duration::from_millis(
        (interval.as_millis() as i64 + offset)
            .max(minimum.as_millis() as i64)
            .max(1) as u64,
    )
}

impl Agent {
    pub(super) fn update_reporting_mode(&self, realtime: bool, initial: bool, now: Instant) {
        let mut reporting = self.reporting.lock().unwrap();
        if initial || !realtime {
            if reporting
                .latest
                .as_ref()
                .is_some_and(|report| report.persisted)
                && reporting.history.is_none()
            {
                self.clear_persisted_pending_report();
            }
            reporting.latest = None;
        }
        if realtime {
            reporting.next_history_due = now;
        }
        if initial {
            if let Some(pending) = reporting.history.as_mut() {
                pending.retry_at = now;
            }
        }
    }

    /// Earliest reporting deadline. A waiting history ACK never delays a live snapshot.
    pub(super) fn next_reporting_deadline(&self) -> Option<Instant> {
        let reporting = self.reporting.lock().unwrap();
        let adaptive = self.adaptive_reporting.lock().unwrap();
        let mut due = reporting.history.as_ref().map(|pending| pending.retry_at);
        if let Some(latest) = &reporting.latest {
            due = Some(due.map_or(latest.retry_at, |at| at.min(latest.retry_at)));
        }
        if adaptive.mode == AdaptiveMode::Realtime && reporting.history.is_none() {
            let history_due = reporting.next_history_due.max(reporting.build_retry_at);
            due = Some(due.map_or(history_due, |at| at.min(history_due)));
        }
        let new_report_due = if adaptive.force_report
            || (adaptive.initial_snapshot_pending && reporting.latest.is_none())
        {
            Some(Instant::now())
        } else if adaptive.mode == AdaptiveMode::Realtime || reporting.history.is_none() {
            adaptive.next_report_due
        } else {
            None
        };
        if let Some(next) = new_report_due {
            let next = next.max(reporting.build_retry_at);
            due = Some(due.map_or(next, |at| at.min(next)));
        }

        due
    }

    pub(super) fn process_adaptive_reporting(&self, cfg: &Config, metrics: &Metrics, now: Instant) {
        // Keep the pending slots occupied through durable cleanup. ACK handling
        // takes this same lock, so a report cannot be replaced during its commit.
        let mut reporting = self.reporting.lock().unwrap();
        let (mode, due, initial, force, generation, interval) = {
            let state = self.adaptive_reporting.lock().unwrap();
            (
                state.mode,
                state.next_report_due.is_some_and(|at| now >= at),
                state.initial_snapshot_pending,
                state.force_report,
                state.generation,
                state.interval,
            )
        };

        let latest_due = now >= reporting.build_retry_at
            && ((initial && reporting.latest.is_none())
                || (mode == AdaptiveMode::Realtime && (force || due)));
        if latest_due {
            let kind = if initial {
                PendingReportKind::InitialSnapshot
            } else {
                PendingReportKind::Realtime
            };
            match self.build_pending_report(cfg, metrics, now, kind, Vec::new(), 0) {
                Ok(mut pending) => {
                    // Current-state frames can be superseded. All historical
                    // observations are retained separately in the durable queue.
                    if reporting.latest.as_ref().is_some_and(|old| old.persisted) {
                        self.clear_persisted_pending_report();
                    }
                    self.dispatch_pending_report(&mut pending, now);
                    reporting.latest = Some(pending);
                    let mut state = self.adaptive_reporting.lock().unwrap();
                    if state.generation == generation {
                        state.force_report = false;
                        state.next_report_due = Some(now + interval);
                    }
                }
                Err(error) => {
                    self.log.warn_once("adaptive-report-build", &error);
                    reporting.build_retry_at = now + Duration::from_secs(2);
                }
            }
        } else if let Some(pending) = reporting.latest.as_mut() {
            if now >= pending.retry_at {
                self.dispatch_pending_report(pending, now);
            }
        }

        if let Some(pending) = reporting.history.as_mut() {
            if now >= pending.retry_at {
                self.dispatch_pending_report(pending, now);
            }
        } else if !initial
            && now >= reporting.build_retry_at
            && ((mode == AdaptiveMode::Realtime && now >= reporting.next_history_due)
                || (mode == AdaptiveMode::IdleBatch && due))
        {
            let batch = self
                .cache
                .lock()
                .unwrap()
                .peek_batch_checked(BATCH_REPORT_SIZE);
            match batch {
                Ok(batch) if !batch.is_empty() => {
                    let values = batch.iter().map(cached_sample_value).collect();
                    match self.build_pending_report(
                        cfg,
                        metrics,
                        now,
                        PendingReportKind::CachedBatch,
                        values,
                        batch.len(),
                    ) {
                        Ok(mut pending) => {
                            self.dispatch_pending_report(&mut pending, now);
                            reporting.history = Some(pending);
                        }
                        Err(error) => {
                            self.log.warn_once("adaptive-report-build", &error);
                            reporting.build_retry_at = now + Duration::from_secs(2);
                        }
                    }
                }
                Ok(_) => {
                    reporting.next_history_due = now + Duration::from_secs(60);
                    if mode == AdaptiveMode::IdleBatch && due {
                        let mut state = self.adaptive_reporting.lock().unwrap();
                        if state.generation == generation {
                            state.next_report_due = Some(
                                now + jittered_idle_interval(interval, ADAPTIVE_SAMPLE_INTERVAL),
                            );
                        }
                    }
                }
                Err(error) => {
                    self.log.warn_once(
                        "cache-read",
                        &format!("historical queue read failed: {}", error),
                    );
                    reporting.next_history_due = now + Duration::from_secs(2);
                    if mode == AdaptiveMode::IdleBatch {
                        self.adaptive_reporting.lock().unwrap().next_report_due =
                            Some(now + Duration::from_secs(2));
                    }
                }
            }
        }
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
            persisted: false,
            generation: self.adaptive_reporting.lock().unwrap().generation,
            source_samples: sample_values
                .iter()
                .filter_map(|sample| serde_json::from_value::<CachedSample>(sample.clone()).ok())
                .collect(),
        })
    }

    fn dispatch_pending_report(&self, pending: &mut PendingReport, now: Instant) {
        if pending.kind.is_batch() && !pending.persisted {
            if let Err(error) = self.persist_pending_report(pending) {
                self.log.warn_once(
                    "adaptive-pending-save",
                    &format!("pending report was not sent: {}", error),
                );
                pending.retry_at = now + Duration::from_secs(2);
                return;
            }
            pending.persisted = true;
        }
        let transport = self.reporter.lock().unwrap().clone();
        let queued = transport
            .as_ref()
            .is_some_and(|transport| transport.send(&pending.body));
        pending.attempts = pending.attempts.saturating_add(1);
        pending.retry_at = now
            + if queued {
                REPORT_ACK_TIMEOUT
            } else {
                Duration::from_secs(2)
            };
    }

    fn persist_pending_report(&self, pending: &PendingReport) -> Result<(), String> {
        let parent = Path::new(&self.pending_report_path)
            .parent()
            .unwrap_or(Path::new("."));
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let persisted = PersistedPendingReport {
            kind: pending.kind,
            body: String::from_utf8(pending.body.clone()).map_err(|e| e.to_string())?,
            sample_count: pending.sample_count,
            consume_count: pending.consume_count,
            attempts: pending.attempts,
            source_samples: pending.source_samples.clone(),
        };
        let encoded = serde_json::to_vec(&persisted).map_err(|e| e.to_string())?;
        let temp_path = format!("{}.tmp", self.pending_report_path);
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path).map_err(|e| e.to_string())?;
        file.write_all(&encoded).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        crate::storage::cache::replace_file(
            Path::new(&temp_path),
            Path::new(&self.pending_report_path),
        )
        .map_err(|e| e.to_string())
    }

    fn clear_persisted_pending_report(&self) {
        if let Err(error) = fs::remove_file(&self.pending_report_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                self.log
                    .warn_once("adaptive-pending-remove", &error.to_string());
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
        let now = Instant::now();
        let mut reporting = self.reporting.lock().unwrap();
        let is_history = reporting
            .history
            .as_ref()
            .is_some_and(|report| report.report_id == report_id);
        let slot = if is_history {
            &mut reporting.history
        } else {
            &mut reporting.latest
        };
        let Some(pending) = slot.as_mut().filter(|report| report.report_id == report_id) else {
            return;
        };
        if !accepted
            || (pending.kind.is_batch()
                && (persisted != Some(true) || accepted_samples != Some(pending.sample_count)))
        {
            pending.retry_at = now + report_retry_delay(pending.attempts);
            drop(reporting);
            self.wake_tick();
            return;
        }
        if pending.kind == PendingReportKind::CachedBatch {
            if let Err(error) = self
                .cache
                .lock()
                .unwrap()
                .acknowledge_batch(&pending.source_samples)
            {
                self.log.warn_once("cache-ack", &error);
                pending.retry_at = now + Duration::from_secs(2);
                return;
            }
        }
        if pending.persisted {
            self.clear_persisted_pending_report();
        }
        let initial = pending.kind == PendingReportKind::InitialSnapshot;
        let generation = pending.generation;
        *slot = None;
        let remaining = if is_history {
            self.cache.lock().unwrap().count()
        } else {
            0
        };
        if is_history {
            reporting.next_history_due = now
                + if remaining >= BATCH_REPORT_SIZE {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(60)
                };
        }
        // Updating the scheduling state is part of the same commit as clearing
        // the slot; the event loop cannot see a half-completed ACK.
        let mut state = self.adaptive_reporting.lock().unwrap();
        if initial {
            state.initial_snapshot_pending = false;
        }
        if state.mode == AdaptiveMode::IdleBatch
            && (initial || is_history)
            && (generation == state.generation || generation == 0)
        {
            state.next_report_due = Some(
                now + if is_history && remaining >= BATCH_REPORT_SIZE {
                    Duration::from_millis(100)
                } else {
                    jittered_idle_interval(state.interval, ADAPTIVE_SAMPLE_INTERVAL)
                },
            );
        }
        drop(state);
        drop(reporting);
        self.note_report_success();
        self.persist_pending_server_id();
        *self.last_report.lock().unwrap() = Some(now);
        self.wake_tick();
    }

    pub(super) fn shrink_pending_report(&self) {
        let mut reporting = self.reporting.lock().unwrap();
        let Some(pending) = reporting.history.as_mut() else {
            return;
        };
        let Some(samples) = pending
            .payload
            .get("samples")
            .and_then(|v| v.as_array())
            .cloned()
        else {
            return;
        };
        if samples.len() <= 1 {
            pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
            self.log.warn_once(
                "wss-report-single-sample-too-large",
                "server rejected a single-sample history report",
            );
            return;
        }
        let mut keep = if samples.len() > 30 {
            30
        } else {
            (samples.len() / 2).max(1)
        };
        loop {
            pending.payload["samples"] =
                serde_json::Value::Array(samples[samples.len() - keep..].to_vec());
            let result = batch_report_id(&pending.payload).and_then(|id| {
                pending.payload["reportId"] = serde_json::Value::String(id.clone());
                serde_json::to_vec(&pending.payload)
                    .map(|body| (id, body))
                    .map_err(|e| e.to_string())
            });
            match result {
                Ok((id, body)) if body.len() <= MAX_REPORT_FRAME_BYTES => {
                    pending.report_id = id;
                    pending.body = body;
                    pending.sample_count = keep;
                    pending.persisted = false;
                    pending.retry_at = Instant::now();
                    break;
                }
                Ok(_) if keep > 1 => keep = (keep / 2).max(1),
                _ => {
                    pending.retry_at = Instant::now() + REPORT_RETRY_MAX;
                    break;
                }
            }
        }
        drop(reporting);
        self.wake_tick();
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

fn body_sample_count(body: &[u8]) -> usize {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("samples").and_then(|s| s.as_array()).map(Vec::len))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "reporter_tests.rs"]
mod tests;
