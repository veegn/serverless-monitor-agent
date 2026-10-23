use crate::metrics::common::*;
use crate::types::*;

pub fn read_cpu_percent() -> (f64, bool) {
    (0.0, false)
}

pub fn collect_basic_stats() -> BasicStats {
    BasicStats {
        mem_total_mb: 0,
        mem_used_mb: 0,
        swap_total_mb: 0,
        swap_used_mb: 0,
        disk_total_mb: 0,
        disk_used_mb: 0,
        disk_devices: Vec::new(),
        load_avg: "0 0 0".to_string(),
        boot_time_ms: 0,
        os_name: std::env::consts::OS.to_string(),
        arch: native_arch().to_string(),
        kernel: String::new(),
        cpu_info: native_arch().to_string(),
        cpu_cores: 1,
        gpu_info: None,
        processes: 0,
        tcp_conn: 0,
        udp_conn: 0,
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    (MemoryStats::default(), false)
}

pub fn read_net_bytes(_ifaces: &str) -> Result<NetBytes, String> {
    Err("unsupported platform".to_string())
}

pub fn read_disk_io_counters(_devices: &[DiskDeviceRef]) -> DiskIOCounters {
    DiskIOCounters::default()
}
