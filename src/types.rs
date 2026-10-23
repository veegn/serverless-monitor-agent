use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

pub const SERVICE_NAME_DEFAULT: &str = "sma";
pub const LEGACY_AGENT_VERSION: &str = "1.0.0";
pub const MAX_TRAFFIC_CORRECTION_GB: f64 = 1_000_000.0;
pub const AUTO_UPDATE_DELAY: Duration = Duration::from_secs(60);
pub const CONFIG_SCHEMA_VERSION: &str = "7";
pub const DEFAULT_REPORT_INTERVAL_SEC: u64 = 60;
pub const DEFAULT_WSS_REPORT_INTERVAL_SEC: u64 = 2;
pub const MAX_BUFFERED_METRIC_SAMPLES: usize = 300;
pub const MIN_WSS_REPORT_INTERVAL_SEC: u64 = 1;
pub const MAX_WSS_REPORT_INTERVAL_SEC: u64 = 5;
pub const CONNECTION_MODE_AUTO: &str = "auto";
pub const CONNECTION_MODE_HTTP: &str = "http";
pub const PING_MODE_TCP: &str = "tcp";
pub const PING_MODE_ICMP: &str = "icmp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub server_id: String,
    pub secret: String,
    pub worker_url: String,
    pub name: String,
    pub collect_interval: u64,
    pub report_interval: u64,
    pub ct_node: String,
    pub cu_node: String,
    pub cm_node: String,
    pub bd_node: String,
    pub node_1: String,
    pub node_2: String,
    pub node_3: String,
    pub node_4: String,
    pub interface: String,
    pub reset_day: u32,
    pub connection_mode: String,
    pub ping_mode: String,
    pub auto_update: bool,
    pub update_proxy: String,
    pub config_md5: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_id: String::new(),
            secret: String::new(),
            worker_url: String::new(),
            name: String::new(),
            collect_interval: 0,
            report_interval: DEFAULT_REPORT_INTERVAL_SEC,
            ct_node: String::new(),
            cu_node: String::new(),
            cm_node: String::new(),
            bd_node: String::new(),
            node_1: String::new(),
            node_2: String::new(),
            node_3: String::new(),
            node_4: String::new(),
            interface: String::new(),
            reset_day: 1,
            connection_mode: CONNECTION_MODE_AUTO.to_string(),
            ping_mode: PING_MODE_TCP.to_string(),
            auto_update: false,
            update_proxy: String::new(),
            config_md5: "none".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Paths {
    pub service_name: String,
    pub binary_file: String,
    pub config_dir: String,
    pub config_file: String,
    pub traffic_file: String,
    pub old_traffic_file: String,
    pub cache_file: String,
    pub pid_file: String,
    pub log_file: String,
    pub service_file: String,
    pub debug_env_file: String,
    pub launchd_label: String,
    pub launchd_user_file: String,
    pub launchd_root_file: String,
    pub user_mode: bool,
    pub run_user: String,
    pub run_uid: i32,
    pub home_dir: String,
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub config: Config,
    pub debug: bool,
    pub rx_correction_gb: String,
    pub tx_correction_gb: String,
    pub explicit: HashSet<String>,
}

impl std::ops::Deref for InstallOptions {
    type Target = Config;
    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl std::ops::DerefMut for InstallOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeResult {
    pub rtt_ms: i32,
    pub loss: i32,
    pub ok: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeSnapshot {
    pub ip_v4: String,
    pub ip_v6: String,
    pub ct: ProbeResult,
    pub cu: ProbeResult,
    pub cm: ProbeResult,
    pub bd: ProbeResult,
    pub node_1: ProbeResult,
    pub node_2: ProbeResult,
    pub node_3: ProbeResult,
    pub node_4: ProbeResult,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskIOStats {
    pub read_bps: u64,
    pub write_bps: u64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub await_ms: f64,
    pub util: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskDeviceRef {
    pub key: String,
    pub major: u64,
    pub minor: u64,
}

#[derive(Debug, Clone, Default)]
pub struct DiskIOCounters {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    pub read_time_ms: u64,
    pub write_time_ms: u64,
    pub io_ticks_ms: u64,
    pub device_count: usize,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default)]
pub struct BasicStats {
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
    pub disk_total_mb: u64,
    pub disk_used_mb: u64,
    pub disk_devices: Vec<DiskDeviceRef>,
    pub load_avg: String,
    pub boot_time_ms: i64,
    pub os_name: String,
    pub arch: String,
    pub kernel: String,
    pub cpu_info: String,
    pub cpu_cores: usize,
    pub gpu_info: Option<serde_json::Value>,
    pub processes: usize,
    pub tcp_conn: usize,
    pub udp_conn: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryStats {
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetBytes {
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReportTime {
    pub local_ts: i64,
    pub accurate_ts: Option<i64>,
    pub offset_ms: Option<i64>,
    pub source: Option<String>,
    pub round_trip_ms: Option<u64>,
    pub sample_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub cpu: String,
    pub ram_total: String,
    pub ram_used: String,
    pub swap_total: String,
    pub swap_used: String,
    pub disk_total: String,
    pub disk_used: String,
    pub disk: DiskIOStats,
    pub load_avg: String,
    pub boot_time: String,
    pub net_rx: String,
    pub net_tx: String,
    pub net_rx_monthly: String,
    pub net_tx_monthly: String,
    pub net_in_speed: String,
    pub net_out_speed: String,
    pub os: String,
    pub arch: String,
    pub kernel_version: String,
    pub cpu_info: String,
    pub cpu_cores: String,
    pub gpu_info: Option<serde_json::Value>,
    pub processes: String,
    pub tcp_conn: String,
    pub udp_conn: String,
    pub ip_v4: String,
    pub ip_v6: String,
    pub ping_ct: serde_json::Value,
    pub ping_cu: serde_json::Value,
    pub ping_cm: serde_json::Value,
    pub ping_bd: serde_json::Value,
    pub ping_node_1: serde_json::Value,
    pub ping_node_2: serde_json::Value,
    pub ping_node_3: serde_json::Value,
    pub ping_node_4: serde_json::Value,
    pub loss_ct: serde_json::Value,
    pub loss_cu: serde_json::Value,
    pub loss_cm: serde_json::Value,
    pub loss_bd: serde_json::Value,
    pub loss_node_1: serde_json::Value,
    pub loss_node_2: serde_json::Value,
    pub loss_node_3: serde_json::Value,
    pub loss_node_4: serde_json::Value,
}
