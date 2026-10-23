use crate::cache::{CachedSample, MetricsCache, BATCH_REPORT_SIZE};
use crate::clock::CalibratedClock;
use crate::command::run_command_quiet;
use crate::config::{
    normalize_config_intervals, normalize_connection_mode, normalize_interface_list,
    normalize_ping_mode, read_config, write_config,
};
use crate::dns::use_public_dns_resolver;
use crate::http::post;
use crate::iplookup::lookup_public_ip;
use crate::log::Logger;
use crate::metrics::{
    collect_basic_stats, disk_io_stats_from_counters, read_cpu_percent, read_disk_io_counters,
    read_memory_stats, read_net_bytes, sample_metrics_to_map,
};
use crate::platform::{
    acquire_instance_lock, default_paths, native_arch, platform_name,
    service_system,
};
use crate::probe::{measure_probe, median_int};
use crate::traffic::{
    apply_traffic_correction_from_network, calc_monthly_traffic, parse_traffic_correction_gb,
};
use crate::transport::{
    is_auth_config_http_status, is_wss_schedule_inactive_reason,
    ReportTransport, TransportHost, AGENT_WSS_MODE_ACTIVE, AGENT_WSS_MODE_DISABLED,
    AGENT_WSS_MODE_HEADER, AGENT_WSS_MODE_INACTIVE, AGENT_WSS_REASON_HEADER, AGENT_WSS_SCHEDULE_INACTIVE,
};
use crate::types::*;
use crate::update::{
    check_latest_update, darwin_update_supported, fetch_update_binary, schedule_update_install,
    UpdateCandidate, AUTO_UPDATE_CHECK_INTERVAL, AUTO_UPDATE_DELAY, AUTO_UPDATE_LOCK_TTL,
};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const METRICS_PROBE_INTERVAL: Duration = Duration::from_secs(20);
pub const METRICS_PROBE_MEDIAN_WINDOW: Duration = Duration::from_secs(120);
pub const METRICS_PROBE_WINDOW_SAMPLE_COUNT: usize = 6;
pub const METRICS_PROBE_SAMPLE_COUNT: usize = 1;
pub const CONFIG_STATE_REPORT_INTERVAL: Duration = Duration::from_secs(60);
pub const REPORT_FAILURE_THRESHOLD: usize = 3;
pub const REPORT_BACKOFF_MAX: Duration = Duration::from_secs(600);
pub const REPORT_FAILURE_DECAY: Duration = Duration::from_secs(1800);

pub const AGENT_AUTO_DISCOVERY_HEADER: &str = "x-sma-auto-discovery";
pub const AGENT_AUTO_DISCOVERY_SERVER_ID_KEY: &str = "x-sma-server-id";
pub const LEGACY_AGENT_AUTO_DISCOVERY_HEADER: &str = "x-cfsm-auto-discovery";
pub const LEGACY_AGENT_AUTO_DISCOVERY_SERVER_ID_KEY: &str = "x-cfsm-server-id";
pub const AGENT_AUTO_DISCOVERY_CREATED: &str = "created";
pub const AGENT_AUTO_DISCOVERY_REUSED: &str = "reused";
pub const AGENT_AUTO_DISCOVERY_FAILED: &str = "failed";
pub const AGENT_AUTO_DISCOVERY_DISABLED: &str = "disabled";

const REPORT_BACKOFF_STEPS: [Duration; 5] = [
    Duration::from_secs(120),
    Duration::from_secs(180),
    Duration::from_secs(300),
    Duration::from_secs(420),
    REPORT_BACKOFF_MAX,
];

#[derive(Debug, PartialEq, Eq)]
pub enum AgentError {
    AutoDiscoveryFailed,
    ConfigError(String),
    LockError(String),
    Other(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::AutoDiscoveryFailed => write!(f, "auto discovery aborted, sma exiting"),
            AgentError::ConfigError(e) => write!(f, "config error: {}", e),
            AgentError::LockError(e) => write!(f, "instance lock failed: {}", e),
            AgentError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for AgentError {}

#[derive(Debug, Clone)]
struct TimedProbeResult {
    at: Instant,
    result: ProbeResult,
}

#[derive(Debug, Clone, Default)]
struct RollingProbeHistory {
    target: String,
    samples: Vec<TimedProbeResult>,
}

impl RollingProbeHistory {
    fn add(&mut self, now: Instant, target: &str, result: ProbeResult) {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            self.target.clear();
            self.samples.clear();
            return;
        }
        if self.target != trimmed {
            self.target = trimmed.to_string();
            self.samples.clear();
        }
        self.samples.push(TimedProbeResult { at: now, result });
        if self.samples.len() > METRICS_PROBE_WINDOW_SAMPLE_COUNT {
            let excess = self.samples.len() - METRICS_PROBE_WINDOW_SAMPLE_COUNT;
            self.samples.drain(0..excess);
        }
    }

    fn snapshot(&self, now: Instant) -> ProbeResult {
        if self.samples.is_empty() {
            return ProbeResult::default();
        }
        let cutoff = now.checked_sub(METRICS_PROBE_MEDIAN_WINDOW).unwrap_or(now);
        let mut values = Vec::new();
        for sample in &self.samples {
            if sample.at >= cutoff && sample.result.ok && sample.result.rtt_ms >= 0 {
                values.push(sample.result.rtt_ms);
            }
        }

        let loss_samples = if self.samples.len() > METRICS_PROBE_WINDOW_SAMPLE_COUNT {
            &self.samples[self.samples.len() - METRICS_PROBE_WINDOW_SAMPLE_COUNT..]
        } else {
            &self.samples[..]
        };

        let mut lost = 0;
        for sample in loss_samples {
            if !sample.result.ok {
                lost += 1;
            }
        }
        let loss = if !loss_samples.is_empty() {
            lost * 100 / loss_samples.len() as i32
        } else {
            0
        };

        if values.is_empty() {
            ProbeResult {
                rtt_ms: -1,
                loss,
                ok: false,
            }
        } else {
            ProbeResult {
                rtt_ms: median_int(&values),
                loss,
                ok: true,
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MetricSample {
    at: Instant,
    metrics: serde_json::Value,
}

pub struct Agent {
    cfg: Arc<RwLock<Config>>,
    paths: Paths,
    log: Logger,
    version: String,
    discovery_failed: Arc<AtomicBool>,
    discovery_just_bound: Arc<AtomicBool>,
    discovery_sync_pending: Arc<AtomicBool>,
    clock: Arc<Mutex<CalibratedClock>>,
    probes: Arc<RwLock<ProbeSnapshot>>,
    basic: Arc<RwLock<BasicStats>>,
    basic_at: Mutex<Option<Instant>>,
    full_at: Mutex<Option<Instant>>,
    prev_net: Mutex<NetBytes>,
    prev_time: Mutex<Option<Instant>>,
    prev_disk: Mutex<DiskIOCounters>,
    prev_disk_at: Mutex<Option<Instant>>,
    disk_io: Mutex<DiskIOStats>,
    monthly_rx: Mutex<u64>,
    monthly_tx: Mutex<u64>,
    samples: Mutex<Vec<MetricSample>>,
    last_sample: Mutex<Option<Instant>>,
    last_report: Mutex<Option<Instant>>,
    last_post: Mutex<Option<Instant>>,
    last_post_attempt: Mutex<Option<Instant>>,
    last_config_state_report_at: Mutex<Option<Instant>>,
    last_config_state_report_md5: Mutex<String>,
    last_config_reject_key: Mutex<String>,
    pending_persist_id: Mutex<String>,
    report_failures: Mutex<usize>,
    report_paused_until: Mutex<Option<Instant>>,
    report_backoff: Mutex<Duration>,
    last_report_failure_at: Mutex<Option<Instant>>,
    wake_cv: Arc<Condvar>,
    wake_mutex: Arc<Mutex<bool>>,
    wss_runtime_disabled_at: Mutex<Option<Instant>>,
    wss_runtime_disabled_reason: Mutex<String>,
    reporter: Mutex<Option<ReportTransport>>,
    cache: Arc<Mutex<MetricsCache>>,
    shutdown: Arc<AtomicBool>,
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
        if let Some(date_time) = crate::clock::parse_http_date(date_str) {
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

pub fn run(config_file: Option<&str>, debug: bool, version: &str) -> Result<(), AgentError> {
    let mut paths = default_paths();
    if let Some(cfg_file) = config_file {
        if !cfg_file.is_empty() {
            paths.config_file = cfg_file.to_string();
            let parent = Path::new(cfg_file).parent().unwrap_or(Path::new(""));
            paths.config_dir = parent.to_string_lossy().to_string();
            paths.traffic_file = parent.join("traffic.dat").to_string_lossy().to_string();
            paths.cache_file = parent.join("metrics_cache.dat").to_string_lossy().to_string();
        }
    }

    let _lock = acquire_instance_lock(&paths).map_err(AgentError::LockError)?;

    let mut cfg = read_config(&paths.config_file).map_err(|e| {
        AgentError::ConfigError(format!("failed to read config {}: {}", paths.config_file, e))
    })?;

    if cfg.secret.is_empty() || cfg.worker_url.is_empty() {
        return Err(AgentError::ConfigError(
            "missing config: SECRET/WORKER_URL cannot be empty".to_string(),
        ));
    }
    if cfg.name.is_empty() && cfg.server_id.is_empty() {
        cfg.name = crate::command::get_system_hostname();
    }
    normalize_config_intervals(&mut cfg);

    let log = Logger::new(debug);
    let agent = Arc::new(Agent {
        cfg: Arc::new(RwLock::new(cfg.clone())),
        paths: paths.clone(),
        log: log.clone(),
        version: version.to_string(),
        discovery_failed: Arc::new(AtomicBool::new(false)),
        discovery_just_bound: Arc::new(AtomicBool::new(false)),
        discovery_sync_pending: Arc::new(AtomicBool::new(false)),
        clock: Arc::new(Mutex::new(CalibratedClock::default())),
        probes: Arc::new(RwLock::new(ProbeSnapshot::default())),
        basic: Arc::new(RwLock::new(collect_basic_stats())),
        basic_at: Mutex::new(Some(Instant::now())),
        full_at: Mutex::new(None),
        prev_net: Mutex::new(NetBytes::default()),
        prev_time: Mutex::new(None),
        prev_disk: Mutex::new(DiskIOCounters::default()),
        prev_disk_at: Mutex::new(None),
        disk_io: Mutex::new(DiskIOStats::default()),
        monthly_rx: Mutex::new(0),
        monthly_tx: Mutex::new(0),
        samples: Mutex::new(Vec::new()),
        last_sample: Mutex::new(None),
        last_report: Mutex::new(None),
        last_post: Mutex::new(None),
        last_post_attempt: Mutex::new(None),
        last_config_state_report_at: Mutex::new(None),
        last_config_state_report_md5: Mutex::new(String::new()),
        last_config_reject_key: Mutex::new(String::new()),
        pending_persist_id: Mutex::new(String::new()),
        report_failures: Mutex::new(0),
        report_paused_until: Mutex::new(None),
        report_backoff: Mutex::new(Duration::ZERO),
        last_report_failure_at: Mutex::new(None),
        wake_cv: Arc::new(Condvar::new()),
        wake_mutex: Arc::new(Mutex::new(false)),
        wss_runtime_disabled_at: Mutex::new(None),
        wss_runtime_disabled_reason: Mutex::new(String::new()),
        reporter: Mutex::new(None),
        cache: Arc::new(Mutex::new(MetricsCache::new(&paths.cache_file))),
        shutdown: Arc::new(AtomicBool::new(false)),
    });

    agent.reset_network_baseline(&cfg.interface);
    let reporter = ReportTransport::new(agent.clone());
    *agent.reporter.lock().unwrap() = Some(reporter.clone());

    log.info(&format!(
        "CF-Server-Monitor Rust Probe started version={} platform={} config={}",
        version,
        platform_name(),
        paths.config_file
    ));
    log.debug(&format!(
        "config id={} url={} report_interval={}s collect_interval={}s reset_day={} connection_mode={} ping_mode={} interface={} auto_update={}",
        cfg.server_id,
        cfg.worker_url,
        cfg.report_interval,
        cfg.collect_interval,
        cfg.reset_day,
        cfg.connection_mode,
        cfg.ping_mode,
        if cfg.interface.is_empty() { "auto" } else { &cfg.interface },
        cfg.auto_update
    ));

    // Spawn network worker
    let agent_net = agent.clone();
    std::thread::Builder::new()
        .name("network-worker".to_string())
        .spawn(move || {
            agent_net.network_worker();
        })
        .ok();

    if agent.uses_wss() {
        reporter.start();
    } else if !cfg.server_id.is_empty() {
        log.info("WSS disabled connection_mode=http");
    }

    if cfg.auto_update {
        let agent_update = agent.clone();
        std::thread::Builder::new()
            .name("update-worker".to_string())
            .spawn(move || {
                agent_update.auto_update_worker();
            })
            .ok();
    } else {
        log.debug("auto update disabled: local AUTO_UPDATE=0");
    }

    let loop_res = agent.event_loop();
    if loop_res == Err(AgentError::AutoDiscoveryFailed) && service_system(&paths) == "procd" {
        let payload = format!("{{\"name\":{:?}}}", paths.service_name);
        let _ = run_command_quiet("ubus", &["call", "service", "delete", &payload]);
    }
    loop_res
}

impl Agent {
    fn event_loop(&self) -> Result<(), AgentError> {
        loop {
            if self.shutdown.load(AtomicOrdering::Relaxed) {
                self.log.info("probe stopped");
                return Ok(());
            }

            self.tick();
            if self.discovery_abandoned() {
                return Err(AgentError::AutoDiscoveryFailed);
            }

            let timeout = self.tick_interval();
            let mut flag = self.wake_mutex.lock().unwrap();
            if !*flag {
                let _ = self.wake_cv.wait_timeout(flag, timeout);
            } else {
                *flag = false;
            }
        }
    }

    fn discovery_abandoned(&self) -> bool {
        self.discovery_failed.load(AtomicOrdering::Relaxed) && self.config_snapshot().server_id.is_empty()
    }

    fn tick_interval(&self) -> Duration {
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

    fn current_wss_report_interval_for_config(&self, cfg: &Config) -> Duration {
        let rep = self.reporter.lock().unwrap().clone();
        if let Some(r) = rep {
            if self.uses_wss_config(cfg) {
                let fallback = Duration::from_secs(DEFAULT_WSS_REPORT_INTERVAL_SEC as u64);
                return r.report_interval(fallback);
            }
        }
        let report_interval = Duration::from_secs(cfg.report_interval as u64);
        if report_interval < Duration::from_secs(1) {
            Duration::from_secs(DEFAULT_REPORT_INTERVAL_SEC as u64)
        } else {
            report_interval
        }
    }

    fn persistent_uses_wss_config(&self, cfg: &Config) -> bool {
        let mode = normalize_connection_mode(&cfg.connection_mode).unwrap_or_else(|_| CONNECTION_MODE_AUTO.to_string());
        mode != CONNECTION_MODE_HTTP
    }

    fn wss_runtime_disabled(&self) -> (bool, String) {
        let disabled_at = self.wss_runtime_disabled_at.lock().unwrap();
        let reason = self.wss_runtime_disabled_reason.lock().unwrap();
        (disabled_at.is_some(), reason.clone())
    }

    fn uses_wss(&self) -> bool {
        self.uses_wss_config(&self.config_snapshot())
    }

    fn uses_wss_config(&self, cfg: &Config) -> bool {
        if cfg.server_id.is_empty() {
            return false;
        }
        if !self.persistent_uses_wss_config(cfg) {
            return false;
        }
        let (disabled, _) = self.wss_runtime_disabled();
        !disabled
    }

    fn report_pause_remaining(&self, now: Instant) -> Duration {
        if let Some(until) = *self.report_paused_until.lock().unwrap() {
            if until > now {
                return until.duration_since(now);
            }
        }
        Duration::ZERO
    }

    fn tick(&self) {
        let now = Instant::now();
        if self.discovery_abandoned() {
            self.samples.lock().unwrap().clear();
            return;
        }

        let cfg = self.config_snapshot();
        if self.report_pause_remaining(now) > Duration::ZERO {
            let collect_interval = effective_collect_interval(&cfg);
            let last_sample = *self.last_sample.lock().unwrap();
            let should_sample = collect_interval > Duration::ZERO
                && (last_sample.is_none() || now.duration_since(last_sample.unwrap()) >= collect_interval);

            if should_sample {
                self.record_offline_sample(now, &cfg);
            }
            return;
        }

        let mut report_interval = Duration::from_secs(cfg.report_interval as u64);
        if report_interval < Duration::from_secs(1) {
            report_interval = Duration::from_secs(DEFAULT_REPORT_INTERVAL_SEC as u64);
        }

        let wss_enabled = self.uses_wss_config(&cfg);
        let rep = self.reporter.lock().unwrap().clone();
        let wss_connected = wss_enabled && rep.as_ref().map(|r| r.connected()).unwrap_or(false);

        let last_report = *self.last_report.lock().unwrap();
        let should_wss_report = wss_connected
            && (last_report.is_none()
                || now.duration_since(last_report.unwrap()) >= self.current_wss_report_interval_for_config(&cfg));

        let last_post = *self.last_post.lock().unwrap();
        let post_due = !wss_connected
            && (last_post.is_none() || now.duration_since(last_post.unwrap()) >= report_interval);

        let post_allowed = !wss_enabled || rep.is_none() || rep.as_ref().unwrap().post_fallback_allowed();
        if post_due && !post_allowed && wss_enabled && rep.is_some() {
            rep.as_ref().unwrap().log_post_fallback_delayed();
        }

        let should_post_report = post_due && post_allowed && self.post_attempt_allowed(now, report_interval);

        let collect_interval = effective_collect_interval(&cfg);
        let last_sample = *self.last_sample.lock().unwrap();
        let should_sample = collect_interval > Duration::ZERO
            && (last_sample.is_none() || now.duration_since(last_sample.unwrap()) >= collect_interval);

        if !should_wss_report && !should_post_report && !should_sample {
            return;
        }

        let report_due = should_wss_report || should_post_report;
        let mut full_at_guard = self.full_at.lock().unwrap();
        let full_due = report_due
            && (full_at_guard.is_none()
                || now.duration_since(full_at_guard.unwrap()) >= report_interval);

        if full_due {
            if crate::log::maybe_rotate_log_file(&self.paths.log_file, crate::log::DEFAULT_MAX_LOG_SIZE) {
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
        let disk_io = self.disk_io.lock().unwrap().clone();

        let m = self.build_metrics(&cfg, &cpu, net_now, rx_speed, tx_speed, rx_monthly, tx_monthly, disk_io);

        if should_sample {
            self.append_sample(now, &m);
        }

        if should_wss_report || should_post_report {
            let res = self.send_report(&cfg, &m, should_wss_report, should_post_report, report_interval);
            self.apply_report_result(res, now, &cfg, &m);
        }
    }

    fn refresh_realtime_basic_stats(&self) {
        let (mem, ok) = read_memory_stats();
        if ok {
            let mut b = self.basic.write().unwrap();
            b.mem_total_mb = mem.mem_total_mb;
            b.mem_used_mb = mem.mem_used_mb;
            b.swap_total_mb = mem.swap_total_mb;
            b.swap_used_mb = mem.swap_used_mb;
        }
    }

    fn reset_network_baseline(&self, iface: &str) {
        let current = read_net_bytes(iface).unwrap_or_default();
        *self.prev_net.lock().unwrap() = current;
        *self.prev_time.lock().unwrap() = Some(Instant::now());
    }

    fn network_rates(
        &self,
        now: Instant,
        current_res: Result<NetBytes, String>,
    ) -> (NetBytes, u64, u64) {
        let mut prev_net_guard = self.prev_net.lock().unwrap();
        let mut prev_time_guard = self.prev_time.lock().unwrap();

        let current = match current_res {
            Ok(c) => c,
            Err(e) => {
                self.log.warn_once("network-counters", &format!("network counters unavailable: {}", e));
                return (*prev_net_guard, 0, 0);
            }
        };

        let mut rx_speed = 0u64;
        let mut tx_speed = 0u64;

        if let Some(prev_time) = *prev_time_guard {
            let elapsed = now.duration_since(prev_time).as_secs_f64();
            if elapsed > 0.0 {
                let rx_delta = counter_delta(prev_net_guard.rx, current.rx);
                let tx_delta = counter_delta(prev_net_guard.tx, current.tx);
                rx_speed = (rx_delta as f64 / elapsed) as u64;
                tx_speed = (tx_delta as f64 / elapsed) as u64;
            }
        }

        *prev_net_guard = current;
        *prev_time_guard = Some(now);
        (current, rx_speed, tx_speed)
    }

    fn sample_disk_io(&self, now: Instant) -> DiskIOStats {
        let devices = self.basic.read().unwrap().disk_devices.clone();
        let current = read_disk_io_counters(&devices);

        let mut prev_guard = self.prev_disk.lock().unwrap();
        let mut prev_at_guard = self.prev_disk_at.lock().unwrap();

        if prev_at_guard.is_none() {
            *prev_guard = current;
            *prev_at_guard = Some(now);
            return DiskIOStats::default();
        }

        let elapsed = now.duration_since(prev_at_guard.unwrap()).as_secs_f64();
        let stats = disk_io_stats_from_counters(prev_guard.clone(), current.clone(), elapsed);
        *prev_guard = current;
        *prev_at_guard = Some(now);
        stats
    }

    fn build_metrics(
        &self,
        cfg: &Config,
        cpu: &str,
        net_now: NetBytes,
        rx_speed: u64,
        tx_speed: u64,
        rx_monthly: u64,
        tx_monthly: u64,
        disk_io: DiskIOStats,
    ) -> Metrics {
        let probes = self.probes.read().unwrap().clone();
        let b = self.basic.read().unwrap().clone();

        Metrics {
            cpu: cpu.to_string(),
            ram_total: b.mem_total_mb.to_string(),
            ram_used: b.mem_used_mb.to_string(),
            swap_total: b.swap_total_mb.to_string(),
            swap_used: b.swap_used_mb.to_string(),
            disk_total: b.disk_total_mb.to_string(),
            disk_used: b.disk_used_mb.to_string(),
            disk: disk_io,
            load_avg: if b.load_avg.is_empty() { "0 0 0".to_string() } else { b.load_avg },
            boot_time: b.boot_time_ms.to_string(),
            net_rx: net_now.rx.to_string(),
            net_tx: net_now.tx.to_string(),
            net_rx_monthly: rx_monthly.to_string(),
            net_tx_monthly: tx_monthly.to_string(),
            net_in_speed: rx_speed.to_string(),
            net_out_speed: tx_speed.to_string(),
            os: if b.os_name.is_empty() { std::env::consts::OS.to_string() } else { b.os_name },
            arch: if b.arch.is_empty() { native_arch() } else { b.arch },
            kernel_version: b.kernel,
            cpu_info: if b.cpu_info.is_empty() { native_arch() } else { b.cpu_info },
            cpu_cores: b.cpu_cores.to_string(),
            gpu_info: b.gpu_info,
            processes: b.processes.to_string(),
            tcp_conn: b.tcp_conn.to_string(),
            udp_conn: b.udp_conn.to_string(),
            ip_v4: if probes.ip_v4.is_empty() { "0".to_string() } else { probes.ip_v4 },
            ip_v6: if probes.ip_v6.is_empty() { "0".to_string() } else { probes.ip_v6 },
            ping_ct: probe_rtt_value(&cfg.ct_node, probes.ct),
            ping_cu: probe_rtt_value(&cfg.cu_node, probes.cu),
            ping_cm: probe_rtt_value(&cfg.cm_node, probes.cm),
            ping_bd: probe_rtt_value(&cfg.bd_node, probes.bd),
            ping_node_1: probe_rtt_value(&cfg.node_1, probes.node_1),
            ping_node_2: probe_rtt_value(&cfg.node_2, probes.node_2),
            ping_node_3: probe_rtt_value(&cfg.node_3, probes.node_3),
            ping_node_4: probe_rtt_value(&cfg.node_4, probes.node_4),
            loss_ct: probe_loss_value(&cfg.ct_node, probes.ct),
            loss_cu: probe_loss_value(&cfg.cu_node, probes.cu),
            loss_cm: probe_loss_value(&cfg.cm_node, probes.cm),
            loss_bd: probe_loss_value(&cfg.bd_node, probes.bd),
            loss_node_1: probe_loss_value(&cfg.node_1, probes.node_1),
            loss_node_2: probe_loss_value(&cfg.node_2, probes.node_2),
            loss_node_3: probe_loss_value(&cfg.node_3, probes.node_3),
            loss_node_4: probe_loss_value(&cfg.node_4, probes.node_4),
        }
    }

    fn append_sample(&self, now: Instant, m: &Metrics) {
        let mut samples = self.samples.lock().unwrap();
        samples.push(MetricSample {
            at: now,
            metrics: sample_metrics_to_map(m),
        });
        if samples.len() > 60 {
            let excess = samples.len() - 60;
            samples.drain(0..excess);
        }
        *self.last_sample.lock().unwrap() = Some(now);
    }

    fn send_report(
        &self,
        cfg: &Config,
        m: &Metrics,
        prefer_wss: bool,
        allow_post: bool,
        report_interval: Duration,
    ) -> ReportSendResult {
        let report_at = Instant::now();
        let (body, sample_count) = match self.build_report_body_for_config(cfg, m, report_at) {
            Ok(res) => res,
            Err(e) => {
                self.log.warn_once("marshal", &format!("marshal payload failed: {}", e));
                return ReportSendResult::default();
            }
        };

        let mut wss_failed = false;
        let rep = self.reporter.lock().unwrap().clone();
        if prefer_wss && rep.is_some() {
            let r = rep.as_ref().unwrap();
            self.log.debug(&format!(
                "WSS report attempt url={} payload_bytes={} samples={}",
                r.url(),
                body.len(),
                sample_count
            ));
            if r.send(&body) {
                return ReportSendResult {
                    ok: true,
                    via_wss: true,
                    via_post: false,
                };
            }
            wss_failed = true;
        }

        if !allow_post && !wss_failed {
            return ReportSendResult::default();
        }

        if prefer_wss && rep.is_some() && !rep.as_ref().unwrap().post_fallback_allowed() {
            rep.as_ref().unwrap().log_post_fallback_delayed();
            return ReportSendResult::default();
        }

        if !self.post_attempt_allowed(report_at, report_interval) {
            return ReportSendResult::default();
        }

        *self.last_post_attempt.lock().unwrap() = Some(report_at);
        let status_code = self.post_report_body(cfg, &body, sample_count, prefer_wss);

        if is_auth_config_http_status(status_code) && rep.is_some() {
            rep.as_ref().unwrap().delay_protocol(&format!("POST fallback http={}", status_code));
        }

        if status_code < 200 || status_code >= 300 {
            if status_code >= 400 {
                self.note_report_failure(status_code);
            }
            return ReportSendResult::default();
        }

        self.note_report_success();
        ReportSendResult {
            ok: true,
            via_wss: false,
            via_post: true,
        }
    }

    fn post_attempt_allowed(&self, now: Instant, report_interval: Duration) -> bool {
        let last = *self.last_post_attempt.lock().unwrap();
        match last {
            Some(t) => now.duration_since(t) >= report_interval,
            None => true,
        }
    }

    fn build_report_body_for_config(
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
                crate::command::get_system_hostname()
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

    fn should_report_config_state(&self, config_md5: &str, report_at: Instant) -> bool {
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

    fn post_report_body(
        &self,
        cfg: &Config,
        body: &[u8],
        sample_count: usize,
        fallback: bool,
    ) -> i32 {
        let label = if fallback { "POST fallback" } else { "report" };
        let req_id = format!("{:012x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros());
        self.log.debug(&format!(
            "{} attempt url={} payload_bytes={} samples={} req_id={}",
            label,
            cfg.worker_url,
            body.len(),
            sample_count,
            req_id
        ));

        let md5 = if cfg.config_md5.is_empty() { "none" } else { &cfg.config_md5 };
        let headers = [
            ("Content-Type", "application/json"),
            ("Accept", "*/*"),
            ("User-Agent", "cfsm"),
            ("X-Agent-Config-Schema", CONFIG_SCHEMA_VERSION),
            ("X-Agent-Version", self.version.as_str()),
            ("X-Agent-Config-Md5", md5),
            ("X-Request-Id", req_id.as_str()),
        ];

        let started = Instant::now();
        let resp = match post(
            &cfg.worker_url,
            &headers,
            body,
            Duration::from_secs(8),
            use_public_dns_resolver(cfg),
        ) {
            Ok(r) => r,
            Err(e) => {
                self.log.error_once(
                    &format!("{}-net-err", label),
                    &format!("{} network request failed [req_id={}]: {}", label, req_id, e),
                );
                self.log.debug(&format!("{} failed: {} req_id={}", label, e, req_id));
                return 0;
            }
        };

        let received = Instant::now();
        self.handle_timed_report_response(
            resp.status as i32,
            &resp.body,
            &resp.headers,
            started,
            received,
            &req_id,
        );
        resp.status as i32
    }

    fn handle_timed_report_response(
        &self,
        status_code: i32,
        resp_body: &[u8],
        headers: &HashMap<String, String>,
        started: Instant,
        received: Instant,
        req_id: &str,
    ) {
        let body_preview = String::from_utf8_lossy(resp_body).trim().to_string();
        self.log.debug(&format!(
            "report response http={} [req_id={}] body={}",
            status_code, req_id, body_preview
        ));

        if status_code >= 400 {
            self.log.error_once(
                &format!("http-status-{}", status_code),
                &format!(
                    "report server response error http={} [req_id={}] body={}",
                    status_code, req_id, body_preview
                ),
            );
        }

        self.apply_auto_discovery_headers(headers);

        if status_code == 404
            && headers.get(AGENT_AUTO_DISCOVERY_HEADER)
                .or_else(|| headers.get(LEGACY_AGENT_AUTO_DISCOVERY_HEADER))
                .map(|s| s.trim())
                .unwrap_or("")
                .is_empty()
            && self.config_snapshot().server_id.is_empty()
        {
            self.abort_auto_discovery(AGENT_AUTO_DISCOVERY_DISABLED, "");
            return;
        }

        if status_code < 200 || status_code >= 300 {
            return;
        }

        self.handle_wss_runtime_headers(headers);

        if let Some(date_str) = headers.get("date") {
            let rtt = received.duration_since(started);
            self.calibrate_date(date_str, rtt);
        }

        let raw_body = String::from_utf8_lossy(resp_body).trim().to_string();
        if raw_body.is_empty() || raw_body == "{}" || raw_body.eq_ignore_ascii_case("OK") {
            return;
        }

        if status_code == 200 {
            if let Err(e) = self.apply_remote_config(resp_body, headers) {
                let mut last_key = self.last_config_reject_key.lock().unwrap();
                if *last_key != e {
                    *last_key = e.clone();
                    self.log.warn(&format!("dynamic configuration rejected: {}", e));
                }
            } else {
                *self.last_config_reject_key.lock().unwrap() = String::new();
            }
        }
    }

    fn handle_wss_runtime_headers(&self, headers: &HashMap<String, String>) {
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

    fn clear_wss_runtime_disabled(&self, reason: &str) {
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

    fn apply_auto_discovery_headers(&self, headers: &HashMap<String, String>) {
        let raw = match headers.get(AGENT_AUTO_DISCOVERY_HEADER)
            .or_else(|| headers.get(LEGACY_AGENT_AUTO_DISCOVERY_HEADER))
        {
            Some(s) => s.trim().to_ascii_lowercase(),
            None => return,
        };
        if raw.is_empty() {
            return;
        }

        let parts: Vec<&str> = raw.splitn(2, ':').collect();
        let status = parts[0];
        let code = if parts.len() > 1 { parts[1] } else { "" };

        match status {
            AGENT_AUTO_DISCOVERY_CREATED => {
                let server_id = headers
                    .get(AGENT_AUTO_DISCOVERY_SERVER_ID_KEY)
                    .or_else(|| headers.get(LEGACY_AGENT_AUTO_DISCOVERY_SERVER_ID_KEY))
                    .map(|s| s.trim())
                    .unwrap_or("");
                if server_id.is_empty() {
                    return;
                }
                let cfg = self.config_snapshot();
                if cfg.server_id == server_id {
                    return;
                }
                let mut next_cfg = cfg;
                next_cfg.server_id = server_id.to_string();

                if let Err(e) = write_config(&self.paths.config_file, next_cfg.clone()) {
                    *self.pending_persist_id.lock().unwrap() = server_id.to_string();
                    self.log.warn(&format!(
                        "failed to persist auto discovery server id (still effective in memory, will retry): {}",
                        e
                    ));
                }
                *self.cfg.write().unwrap() = next_cfg;
                self.log.info("auto discovery created server successfully, please check dashboard after 2 minutes");
                self.discovery_just_bound.store(true, AtomicOrdering::Relaxed);
                *self.last_post_attempt.lock().unwrap() = None;
                if let Some(r) = self.reporter.lock().unwrap().as_ref() {
                    r.reset_report_interval();
                }
                self.wake_tick();
            }
            AGENT_AUTO_DISCOVERY_REUSED => {
                self.abort_auto_discovery(AGENT_AUTO_DISCOVERY_REUSED, "");
            }
            AGENT_AUTO_DISCOVERY_FAILED => {
                let detail = if code.is_empty() { "unknown" } else { code };
                self.abort_auto_discovery(AGENT_AUTO_DISCOVERY_FAILED, detail);
            }
            _ => {}
        }
    }

    fn abort_auto_discovery(&self, kind: &str, detail: &str) {
        if self.discovery_failed.swap(true, AtomicOrdering::Relaxed) {
            return;
        }
        match kind {
            AGENT_AUTO_DISCOVERY_REUSED => {
                self.log.warn("auto discovery aborted: server with same name already exists; please retry with another -name or install with panel -id");
            }
            AGENT_AUTO_DISCOVERY_DISABLED => {
                self.log.warn("auto discovery disabled on server: please enable auto-discovery in dashboard settings");
            }
            _ => {
                self.log.warn(&format!(
                    "auto discovery failed: code={}, aborting. {}",
                    detail,
                    auto_discovery_failure_hint(detail)
                ));
            }
        }
    }

    fn record_offline_sample(&self, now: Instant, cfg: &Config) {
        self.refresh_realtime_basic_stats();
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
        let rx_monthly = *self.monthly_rx.lock().unwrap();
        let tx_monthly = *self.monthly_tx.lock().unwrap();
        let disk_io = self.disk_io.lock().unwrap().clone();
        let m = self.build_metrics(cfg, &cpu, net_now, rx_speed, tx_speed, rx_monthly, tx_monthly, disk_io);

        let (ts, _) = self.clock.lock().unwrap().timestamp(now);
        let sample = CachedSample {
            ts,
            metrics: sample_metrics_to_map(&m),
        };
        let _ = self.cache.lock().unwrap().append_sample(&sample);
        *self.last_sample.lock().unwrap() = Some(now);
    }

    fn apply_report_result(&self, result: ReportSendResult, now: Instant, cfg: &Config, m: &Metrics) {
        self.persist_pending_server_id();
        if self.discovery_just_bound.swap(false, AtomicOrdering::Relaxed) {
            self.samples.lock().unwrap().clear();
            self.discovery_sync_pending.store(true, AtomicOrdering::Relaxed);
            return;
        }

        if result.ok {
            if result.via_wss {
                *self.last_report.lock().unwrap() = Some(now);
                *self.last_post.lock().unwrap() = Some(now);
            } else if result.via_post {
                *self.last_post.lock().unwrap() = Some(now);
            }
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

    fn drain_cached_samples(&self, cfg: &Config) {
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
            if self.uses_wss_config(cfg) && rep.is_some() && rep.as_ref().unwrap().connected() {
                sent = rep.as_ref().unwrap().send(&body);
            }

            if !sent {
                let status = self.post_report_body(cfg, &body, batch_len, false);
                sent = (200..300).contains(&status);
            }

            if sent {
                let _ = self.cache.lock().unwrap().drain(batch_len);
                let remaining = self.cache.lock().unwrap().count();
                self.log.info(&format!(
                    "batch reported {} cached samples, remaining: {}",
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

    fn persist_pending_server_id(&self) {
        let mut pending = self.pending_persist_id.lock().unwrap();
        if pending.is_empty() {
            return;
        }
        let cfg = self.config_snapshot();
        if write_config(&self.paths.config_file, cfg).is_ok() {
            *pending = String::new();
            self.log.info(&format!(
                "auto discovery server id persisted to {}, no need to re-register after restart",
                self.paths.config_file
            ));
        }
    }

    fn sync_report_transport(&self) {
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

    pub fn apply_remote_config(&self, body: &[u8], headers: &HashMap<String, String>) -> Result<(), String> {
        self.apply_remote_config_with_options(body, headers, false)
    }

    pub fn apply_remote_config_with_options(
        &self,
        body: &[u8],
        headers: &HashMap<String, String>,
        allow_missing_md5: bool,
    ) -> Result<(), String> {
        if body.is_empty() {
            return Err("empty body".to_string());
        }
        if body.len() > 1024 {
            return Err("response too large".to_string());
        }
        let raw = String::from_utf8_lossy(body).trim().to_string();
        if raw.is_empty() {
            return Err("empty body".to_string());
        }

        let re = Regex::new(r"^[A-Za-z0-9_=&.,:%+\-\*\?\[\]]*$").unwrap();
        if !re.is_match(&raw) {
            return Err("invalid body characters".to_string());
        }

        let parsed_pairs = url::form_urlencoded::parse(raw.as_bytes());
        let mut values: HashMap<String, String> = HashMap::new();
        for (k, v) in parsed_pairs {
            values.insert(k.to_string(), v.to_string());
        }

        let allowed = [
            "collect_interval", "report_interval", "wss_report_interval", "reset_day",
            "schema_version", "custom_ct", "custom_cu", "custom_cm", "custom_bd",
            "node_1", "node_2", "node_3", "node_4", "interface", "connection_mode",
            "ping_mode", "rx_correction", "tx_correction", "update",
        ];
        for k in values.keys() {
            if !allowed.contains(&k.as_str()) {
                return Err(format!("unknown field {}", k));
            }
        }

        if let Some(upd) = values.get("update") {
            if !upd.is_empty() && upd != "0" && upd != "1" {
                return Err(format!("invalid update {}", upd));
            }
        }

        let has_config = values.contains_key("collect_interval")
            || values.contains_key("report_interval")
            || values.contains_key("wss_report_interval")
            || values.contains_key("reset_day")
            || values.contains_key("schema_version")
            || values.contains_key("interface")
            || values.contains_key("connection_mode")
            || values.contains_key("ping_mode")
            || values.contains_key("node_1")
            || values.contains_key("node_2")
            || values.contains_key("node_3")
            || values.contains_key("node_4");

        let has_correction = values.contains_key("rx_correction") || values.contains_key("tx_correction");
        let cfg = self.config_snapshot();

        if !has_config {
            if has_correction {
                let rx = values.get("rx_correction").cloned().unwrap_or_default();
                let tx = values.get("tx_correction").cloned().unwrap_or_default();
                apply_traffic_correction_from_network(&self.paths.traffic_file, &cfg.interface, &rx, &tx)?;
                let _ = self.send_correction_confirm(&cfg, &rx, &tx);
                return Ok(());
            }
            if values.contains_key("update") {
                return Ok(());
            }
            return Err("no config fields".to_string());
        }

        let new_md5 = headers
            .get("X-Agent-Config-Md5")
            .or_else(|| headers.get("x-agent-config-md5"))
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let has_remote_md5 = valid_config_md5(&new_md5);

        if !has_remote_md5 && !allow_missing_md5 {
            return Err("invalid remote md5".to_string());
        }

        let collect = values.get("collect_interval").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1);
        let report = values.get("report_interval").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1);
        let wss_report = values.get("wss_report_interval").and_then(|s| s.parse::<i32>().ok()).unwrap_or(DEFAULT_WSS_REPORT_INTERVAL_SEC as i32);
        let reset = values.get("reset_day").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1);

        if ![0, 1, 2, 5, 10].contains(&collect) {
            return Err(format!("invalid collect_interval {}", collect));
        }
        if ![30, 60, 120, 180].contains(&report) {
            return Err(format!("invalid report_interval {}", report));
        }
        if wss_report < MIN_WSS_REPORT_INTERVAL_SEC as i32 || wss_report > MAX_WSS_REPORT_INTERVAL_SEC as i32 {
            return Err(format!("invalid wss_report_interval {}", wss_report));
        }
        if reset < 0 || reset > 31 {
            return Err(format!("invalid reset_day {}", reset));
        }
        if values.get("schema_version").map(|s| s.as_str()) != Some(CONFIG_SCHEMA_VERSION) {
            return Err(format!("invalid schema_version {:?}", values.get("schema_version")));
        }
        if report < collect {
            return Err("report_interval less than collect_interval".to_string());
        }

        let iface = normalize_interface_list(values.get("interface").map(|s| s.as_str()).unwrap_or(""))?;
        let connection_mode = normalize_connection_mode(values.get("connection_mode").map(|s| s.as_str()).unwrap_or(""))?;
        let ping_mode = normalize_ping_mode(values.get("ping_mode").map(|s| s.as_str()).unwrap_or(""))?;

        let mut effective_collect = collect;
        if connection_mode == CONNECTION_MODE_AUTO && (collect == 0 || collect > wss_report) {
            effective_collect = wss_report;
        }

        let should_apply = if has_remote_md5 {
            new_md5 != cfg.config_md5
        } else {
            remote_config_differs(&cfg, &values, effective_collect, report, reset, &iface, &connection_mode, &ping_mode)
        };

        if should_apply {
            let mut next_cfg = cfg.clone();
            next_cfg.collect_interval = effective_collect as u64;
            next_cfg.report_interval = report as u64;
            next_cfg.reset_day = reset as u32;
            if let Some(v) = values.get("custom_ct") { next_cfg.ct_node = v.clone(); }
            if let Some(v) = values.get("custom_cu") { next_cfg.cu_node = v.clone(); }
            if let Some(v) = values.get("custom_cm") { next_cfg.cm_node = v.clone(); }
            if let Some(v) = values.get("custom_bd") { next_cfg.bd_node = v.clone(); }
            if let Some(v) = values.get("node_1") { next_cfg.node_1 = v.clone(); }
            if let Some(v) = values.get("node_2") { next_cfg.node_2 = v.clone(); }
            if let Some(v) = values.get("node_3") { next_cfg.node_3 = v.clone(); }
            if let Some(v) = values.get("node_4") { next_cfg.node_4 = v.clone(); }
            next_cfg.interface = iface.clone();
            next_cfg.connection_mode = connection_mode;
            next_cfg.ping_mode = ping_mode;
            if has_remote_md5 {
                next_cfg.config_md5 = new_md5.clone();
            }

            write_config(&self.paths.config_file, next_cfg.clone()).map_err(|e| e.to_string())?;
            *self.cfg.write().unwrap() = next_cfg.clone();

            self.reset_network_baseline(&next_cfg.interface);
            *self.prev_disk.lock().unwrap() = DiskIOCounters::default();
            *self.prev_disk_at.lock().unwrap() = None;
            *self.disk_io.lock().unwrap() = DiskIOStats::default();
            *self.monthly_rx.lock().unwrap() = 0;
            *self.monthly_tx.lock().unwrap() = 0;
            *self.full_at.lock().unwrap() = None;
            self.samples.lock().unwrap().clear();
            *self.last_sample.lock().unwrap() = None;
            *self.last_report.lock().unwrap() = None;
            *self.last_post.lock().unwrap() = None;
            *self.last_post_attempt.lock().unwrap() = None;
            *self.last_config_state_report_at.lock().unwrap() = None;
            *self.last_config_state_report_md5.lock().unwrap() = String::new();

            if let Some(r) = self.reporter.lock().unwrap().as_ref() {
                r.reset_report_interval();
            }
            self.sync_report_transport();
            self.wake_tick();

            self.log.info(&format!(
                "dynamic configuration applied md5={} connection_mode={} ping_mode={} interface={}",
                if next_cfg.config_md5.is_empty() { "none" } else { &next_cfg.config_md5 },
                next_cfg.connection_mode,
                next_cfg.ping_mode,
                if iface.is_empty() { "auto" } else { &iface }
            ));
        }

        if has_correction {
            let rx = values.get("rx_correction").cloned().unwrap_or_default();
            let tx = values.get("tx_correction").cloned().unwrap_or_default();
            apply_traffic_correction_from_network(&self.paths.traffic_file, &cfg.interface, &rx, &tx)?;
            let _ = self.send_correction_confirm(&cfg, &rx, &tx);
        }

        Ok(())
    }

    fn send_correction_confirm(&self, cfg: &Config, rx: &str, tx: &str) -> Result<(), String> {
        let _ = parse_traffic_correction_gb(rx)?;
        let _ = parse_traffic_correction_gb(tx)?;
        let rx_val: f64 = rx.trim().parse().unwrap_or(0.0);
        let tx_val: f64 = tx.trim().parse().unwrap_or(0.0);

        let payload = serde_json::json!({
            "id": cfg.server_id,
            "secret": cfg.secret,
            "rx_correction": rx_val,
            "tx_correction": tx_val,
        });

        let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        let headers = [
            ("Content-Type", "application/json"),
            ("Accept", "*/*"),
            ("User-Agent", "cfsm"),
        ];

        let resp = post(
            &cfg.worker_url,
            &headers,
            &body,
            Duration::from_secs(4),
            use_public_dns_resolver(cfg),
        )?;
        if resp.status < 200 || resp.status >= 300 {
            return Err(format!("http {}", resp.status));
        }
        self.log.info(&format!(
            "traffic correction confirm sent rx={}GB tx={}GB",
            if rx.is_empty() { "0" } else { rx },
            if tx.is_empty() { "0" } else { tx }
        ));
        Ok(())
    }

    fn network_worker(&self) {
        let mut last_ip: Option<Instant> = None;
        let mut last_probe: Option<Instant> = None;

        let mut ct_hist = RollingProbeHistory::default();
        let mut cu_hist = RollingProbeHistory::default();
        let mut cm_hist = RollingProbeHistory::default();
        let mut bd_hist = RollingProbeHistory::default();
        let mut node1_hist = RollingProbeHistory::default();
        let mut node2_hist = RollingProbeHistory::default();
        let mut node3_hist = RollingProbeHistory::default();
        let mut node4_hist = RollingProbeHistory::default();

        loop {
            if self.shutdown.load(AtomicOrdering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_secs(5));
            let now = Instant::now();
            let cfg = self.config_snapshot();

            let mut snap = ProbeSnapshot::default();
            let mut need_update = false;

            if last_ip.is_none() || now.duration_since(last_ip.unwrap()) >= Duration::from_secs(600) {
                let use_dns = use_public_dns_resolver(&cfg);
                snap.ip_v4 = lookup_public_ip("tcp4", &self.log, use_dns);
                snap.ip_v6 = lookup_public_ip("tcp6", &self.log, use_dns);
                last_ip = Some(now);
                need_update = true;
            }

            if last_probe.is_none() || now.duration_since(last_probe.unwrap()) >= METRICS_PROBE_INTERVAL {
                ct_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.ct_node), measure_probe(&cfg.ping_mode, &cfg.ct_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                cu_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.cu_node), measure_probe(&cfg.ping_mode, &cfg.cu_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                cm_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.cm_node), measure_probe(&cfg.ping_mode, &cfg.cm_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                bd_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.bd_node), measure_probe(&cfg.ping_mode, &cfg.bd_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node1_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_1), measure_probe(&cfg.ping_mode, &cfg.node_1, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node2_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_2), measure_probe(&cfg.ping_mode, &cfg.node_2, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node3_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_3), measure_probe(&cfg.ping_mode, &cfg.node_3, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node4_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_4), measure_probe(&cfg.ping_mode, &cfg.node_4, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));

                snap.ct = ct_hist.snapshot(now);
                snap.cu = cu_hist.snapshot(now);
                snap.cm = cm_hist.snapshot(now);
                snap.bd = bd_hist.snapshot(now);
                snap.node_1 = node1_hist.snapshot(now);
                snap.node_2 = node2_hist.snapshot(now);
                snap.node_3 = node3_hist.snapshot(now);
                snap.node_4 = node4_hist.snapshot(now);

                last_probe = Some(now);
                need_update = true;
            }

            if need_update {
                let mut probes = self.probes.write().unwrap();
                if snap.ip_v4.is_empty() { snap.ip_v4 = probes.ip_v4.clone(); }
                if snap.ip_v6.is_empty() { snap.ip_v6 = probes.ip_v6.clone(); }
                if snap.ct == ProbeResult::default() { snap.ct = probes.ct; }
                if snap.cu == ProbeResult::default() { snap.cu = probes.cu; }
                if snap.cm == ProbeResult::default() { snap.cm = probes.cm; }
                if snap.bd == ProbeResult::default() { snap.bd = probes.bd; }
                if snap.node_1 == ProbeResult::default() { snap.node_1 = probes.node_1; }
                if snap.node_2 == ProbeResult::default() { snap.node_2 = probes.node_2; }
                if snap.node_3 == ProbeResult::default() { snap.node_3 = probes.node_3; }
                if snap.node_4 == ProbeResult::default() { snap.node_4 = probes.node_4; }
                *probes = snap;
            }
        }
    }

    fn auto_update_worker(&self) {
        self.check_and_schedule_agent_update("startup");
        loop {
            if self.shutdown.load(AtomicOrdering::Relaxed) {
                return;
            }
            std::thread::sleep(AUTO_UPDATE_CHECK_INTERVAL);
            self.check_and_schedule_agent_update("periodic");
        }
    }

    fn check_and_schedule_agent_update(&self, reason: &str) {
        if cfg!(target_os = "macos") {
            let (ok, ver) = darwin_update_supported();
            if !ok {
                self.log.info(&format!(
                    "auto update skipped: current builds require macOS 13 Ventura or later reason={} version={} macos={}",
                    reason, self.version, ver
                ));
                return;
            }
        }

        let cfg = self.config_snapshot();
        let use_dns = use_public_dns_resolver(&cfg);
        let candidate_opt = match check_latest_update(&self.version, use_dns) {
            Ok(c) => c,
            Err(e) => {
                self.log.info(&format!("auto update check failed reason={}: {}", reason, e));
                return;
            }
        };

        let candidate = match candidate_opt {
            Some(c) => c,
            None => {
                self.log.info(&format!(
                    "auto update checked: current version is up to date reason={} version={}",
                    reason, self.version
                ));
                return;
            }
        };

        self.schedule_agent_update(&candidate, reason, &cfg);
    }

    fn schedule_agent_update(&self, candidate: &UpdateCandidate, reason: &str, cfg: &Config) {
        let lock_file = Path::new(&self.paths.config_dir).join("auto_update.lock");
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;

        if let Ok(data) = fs::read_to_string(&lock_file) {
            if let Ok(last) = data.trim().parse::<i64>() {
                if now_unix - last < AUTO_UPDATE_LOCK_TTL.as_secs() as i64 {
                    self.log.info("auto update already scheduled recently");
                    return;
                }
            }
        }

        let use_dns = use_public_dns_resolver(cfg);
        let bin_path = match fetch_update_binary(&self.paths.config_dir, candidate, &cfg.update_proxy, use_dns) {
            Ok(p) => p,
            Err(e) => {
                self.log.info(&format!(
                    "auto update download failed target={}: {}",
                    candidate.tag_name, e
                ));
                return;
            }
        };

        let method = match schedule_update_install(&self.paths, &bin_path, now_unix) {
            Ok(m) => m,
            Err(e) => {
                let _ = fs::remove_file(&bin_path);
                self.log.info(&format!("schedule update failed: {}", e));
                return;
            }
        };

        let _ = fs::create_dir_all(&self.paths.config_dir);
        let _ = fs::write(&lock_file, now_unix.to_string().as_bytes());
        self.log.info(&format!(
            "auto update scheduled target={} asset={} method={} reason={} delay={}s",
            candidate.tag_name,
            candidate.asset_name,
            method,
            reason,
            AUTO_UPDATE_DELAY.as_secs()
        ));
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ReportSendResult {
    ok: bool,
    via_wss: bool,
    via_post: bool,
}

fn probe_history_key(ping_mode: &str, node: &str) -> String {
    format!("{}:{}", ping_mode, node.trim())
}

fn probe_rtt_value(node: &str, r: ProbeResult) -> serde_json::Value {
    if node.trim().is_empty() {
        serde_json::json!(false)
    } else if !r.ok || r.rtt_ms < 0 {
        serde_json::json!("null")
    } else {
        serde_json::json!(r.rtt_ms.to_string())
    }
}

fn probe_loss_value(node: &str, r: ProbeResult) -> serde_json::Value {
    if node.trim().is_empty() {
        serde_json::json!(false)
    } else if r.loss < 0 {
        serde_json::json!("100")
    } else {
        serde_json::json!(r.loss.to_string())
    }
}

fn valid_config_md5(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn remote_config_differs(
    cfg: &Config,
    values: &HashMap<String, String>,
    collect: i32,
    report: i32,
    reset: i32,
    iface: &str,
    connection_mode: &str,
    ping_mode: &str,
) -> bool {
    (cfg.collect_interval as i64) != (collect as i64)
        || (cfg.report_interval as i64) != (report as i64)
        || (cfg.reset_day as i64) != (reset as i64)
        || cfg.ct_node != values.get("custom_ct").map(|s| s.as_str()).unwrap_or("")
        || cfg.cu_node != values.get("custom_cu").map(|s| s.as_str()).unwrap_or("")
        || cfg.cm_node != values.get("custom_cm").map(|s| s.as_str()).unwrap_or("")
        || cfg.bd_node != values.get("custom_bd").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_1 != values.get("node_1").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_2 != values.get("node_2").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_3 != values.get("node_3").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_4 != values.get("node_4").map(|s| s.as_str()).unwrap_or("")
        || cfg.interface != iface
        || cfg.connection_mode != connection_mode
        || cfg.ping_mode != ping_mode
}

fn effective_collect_interval(cfg: &Config) -> Duration {
    if cfg.collect_interval <= 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(cfg.collect_interval as u64)
    }
}

fn duration_gcd(mut a: Duration, mut b: Duration) -> Duration {
    while !b.is_zero() {
        let remainder = Duration::from_nanos(a.as_nanos() as u64 % b.as_nanos() as u64);
        a = b;
        b = remainder;
    }
    a
}

fn counter_delta(prev: u64, current: u64) -> u64 {
    if current >= prev {
        current - prev
    } else {
        current
    }
}

fn report_backoff_for_failures(failures: usize) -> Duration {
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

fn auto_discovery_failure_hint(code: &str) -> &'static str {
    match code {
        "no_name" => "server cannot obtain server name, please specify with -name=NAME or install with panel -id",
        "db_error" => "server database error during registration, please check Worker logs or install with panel -id",
        _ => "please check Worker logs or install with panel -id",
    }
}
