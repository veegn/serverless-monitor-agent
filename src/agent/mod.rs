//! Agent runtime orchestration.
//!
//! Submodules:
//! - [`collector`]: Metric sampling, CPU/RAM/disk/network collection
//! - [`discovery`]: Agent auto-registration with the server
//! - [`error`]: Agent error types
//! - [`host`]: `TransportHost` implementation for WebSocket session
//! - [`probe_worker`]: Background ping and probe measurements
//! - [`remote_config`]: Dynamic configuration and traffic corrections
//! - [`reporter`]: Report serialization, dispatch, and offline batch replay
//! - [`scheduler`]: Main event loop timing and interval dispatch
//! - [`update_worker`]: Self-update checks and install scheduling

mod collector;
mod discovery;
pub mod error;
mod host;
mod probe_worker;
mod remote_config;
mod reporter;
mod scheduler;
mod update_worker;

pub use discovery::{
    AGENT_AUTO_DISCOVERY_CREATED, AGENT_AUTO_DISCOVERY_DISABLED, AGENT_AUTO_DISCOVERY_FAILED,
    AGENT_AUTO_DISCOVERY_HEADER, AGENT_AUTO_DISCOVERY_REUSED, AGENT_AUTO_DISCOVERY_SERVER_ID_KEY,
};
pub use error::AgentError;
pub use host::{REPORT_BACKOFF_MAX, REPORT_FAILURE_DECAY, REPORT_FAILURE_THRESHOLD};
pub use probe_worker::{
    METRICS_PROBE_INTERVAL, METRICS_PROBE_MEDIAN_WINDOW, METRICS_PROBE_SAMPLE_COUNT,
    METRICS_PROBE_WINDOW_SAMPLE_COUNT,
};
pub use reporter::CONFIG_STATE_REPORT_INTERVAL;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::metrics::collect_basic_stats;
use crate::platform::{acquire_instance_lock, default_paths, platform_name, service_system};
use crate::storage::cache::MetricsCache;
use crate::storage::config::{normalize_config_intervals, read_config};
use crate::transport::ReportTransport;
use crate::types::{
    BasicStats, Config, DiskIOCounters, DiskIOStats, NetBytes, Paths, ProbeSnapshot,
};
use crate::util::clock::CalibratedClock;
use crate::util::command::run_command_quiet;
use crate::util::log::Logger;

#[derive(Debug, Clone)]
pub(super) struct MetricSample {
    pub(super) at: Instant,
    pub(super) metrics: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdaptiveMode {
    Realtime,
    IdleBatch,
}

pub(super) struct AdaptiveReportingState {
    pub(super) mode: AdaptiveMode,
    pub(super) interval: Duration,
    pub(super) mode_revision: u64,
    pub(super) generation: u64,
    pub(super) next_report_due: Option<Instant>,
    pub(super) force_report: bool,
    pub(super) initial_snapshot_pending: bool,
}

impl Default for AdaptiveReportingState {
    fn default() -> Self {
        Self {
            mode: AdaptiveMode::Realtime,
            interval: Duration::from_secs(crate::types::DEFAULT_WSS_REPORT_INTERVAL_SEC),
            mode_revision: 0,
            generation: 0,
            next_report_due: None,
            force_report: false,
            initial_snapshot_pending: false,
        }
    }
}

pub struct Agent {
    pub(super) cfg: Arc<RwLock<Config>>,
    pub(super) paths: Paths,
    pub(super) log: Logger,
    pub(super) version: String,
    pub(super) discovery_failed: Arc<AtomicBool>,
    pub(super) discovery_just_bound: Arc<AtomicBool>,
    pub(super) discovery_sync_pending: Arc<AtomicBool>,
    pub(super) clock: Arc<Mutex<CalibratedClock>>,
    pub(super) probes: Arc<RwLock<ProbeSnapshot>>,
    pub(super) basic: Arc<RwLock<BasicStats>>,
    pub(super) basic_at: Mutex<Option<Instant>>,
    pub(super) full_at: Mutex<Option<Instant>>,
    pub(super) prev_net: Mutex<NetBytes>,
    pub(super) prev_time: Mutex<Option<Instant>>,
    pub(super) prev_disk: Mutex<DiskIOCounters>,
    pub(super) prev_disk_at: Mutex<Option<Instant>>,
    pub(super) disk_io: Mutex<DiskIOStats>,
    pub(super) monthly_rx: Mutex<u64>,
    pub(super) monthly_tx: Mutex<u64>,
    pub(super) samples: Mutex<Vec<MetricSample>>,
    pub(super) adaptive_reporting: Mutex<AdaptiveReportingState>,
    pending_report_path: String,
    pending_report: Mutex<Option<reporter::PendingReport>>,
    pub(super) last_sample: Mutex<Option<Instant>>,
    pub(super) last_report: Mutex<Option<Instant>>,
    pub(super) last_config_state_report_at: Mutex<Option<Instant>>,
    pub(super) last_config_state_report_md5: Mutex<String>,
    pub(super) last_config_reject_key: Mutex<String>,
    pub(super) pending_persist_id: Mutex<String>,
    pub(super) report_failures: Mutex<usize>,
    pub(super) report_paused_until: Mutex<Option<Instant>>,
    pub(super) report_backoff: Mutex<Duration>,
    pub(super) last_report_failure_at: Mutex<Option<Instant>>,
    pub(super) wake_cv: Arc<Condvar>,
    pub(super) wake_mutex: Arc<Mutex<bool>>,
    pub(super) wss_runtime_disabled_at: Mutex<Option<Instant>>,
    pub(super) wss_runtime_disabled_reason: Mutex<String>,
    pub(super) reporter: Mutex<Option<ReportTransport>>,
    pub(super) cache: Arc<Mutex<MetricsCache>>,
    pub(super) shutdown: Arc<AtomicBool>,
}

pub fn run(config_file: Option<&str>, debug: bool, version: &str) -> Result<(), AgentError> {
    let mut paths = default_paths();
    if let Some(cfg_file) = config_file {
        if !cfg_file.is_empty() {
            paths.config_file = cfg_file.to_string();
            let parent = Path::new(cfg_file).parent().unwrap_or(Path::new(""));
            paths.config_dir = parent.to_string_lossy().to_string();
            paths.traffic_file = parent.join("traffic.dat").to_string_lossy().to_string();
            paths.cache_file = parent
                .join("metrics_cache.dat")
                .to_string_lossy()
                .to_string();
        }
    }

    let _lock = acquire_instance_lock(&paths).map_err(AgentError::LockError)?;

    let mut cfg = read_config(&paths.config_file).map_err(|e| {
        AgentError::ConfigError(format!(
            "failed to read config {}: {}",
            paths.config_file, e
        ))
    })?;

    if cfg.secret.is_empty() || cfg.worker_url.is_empty() {
        return Err(AgentError::ConfigError(
            "missing config: SECRET/WORKER_URL cannot be empty".to_string(),
        ));
    }
    if cfg.name.is_empty() && cfg.server_id.is_empty() {
        cfg.name = crate::util::command::get_system_hostname();
    }
    normalize_config_intervals(&mut cfg);

    let log = Logger::new(debug);
    let pending_report_path = format!("{}.pending", paths.cache_file);
    let restored_pending_report = reporter::restore_pending_report(&pending_report_path);
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
        adaptive_reporting: Mutex::new(AdaptiveReportingState::default()),
        pending_report_path,
        pending_report: Mutex::new(restored_pending_report),
        last_sample: Mutex::new(None),
        last_report: Mutex::new(None),
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
    pub(super) fn config_snapshot(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    pub(super) fn wake_tick(&self) {
        let mut flag = self.wake_mutex.lock().unwrap();
        *flag = true;
        self.wake_cv.notify_all();
    }

    pub(super) fn event_loop(&self) -> Result<(), AgentError> {
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
}
