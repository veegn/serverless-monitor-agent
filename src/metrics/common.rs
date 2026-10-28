use crate::types::*;
use crate::util::command::{command_exists, command_output};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuMetric {
    pub name: String,
    pub info: serde_json::Value,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_used: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_total: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sm_clock: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<serde_json::Value>,
}

pub fn uint_string(v: u64) -> String {
    v.to_string()
}

pub fn int_string(v: usize) -> String {
    v.to_string()
}

pub fn float_string(v: f64) -> String {
    let clamped = v.clamp(0.0, 100.0);
    format!("{:.2}", clamped)
}

pub fn cpu_usage_percent(prev: CpuTimes, current: CpuTimes) -> Option<f64> {
    if current.total < prev.total || current.idle < prev.idle {
        return None;
    }
    let total_delta = current.total - prev.total;
    let idle_delta = current.idle - prev.idle;
    if total_delta == 0 || idle_delta > total_delta {
        return None;
    }
    Some(((total_delta - idle_delta) as f64 / total_delta as f64) * 100.0)
}

pub fn cpu_percent_string(v: f64) -> String {
    float_string(v)
}

pub fn disk_usage_mb_from_blocks(blocks: u64, bfree: u64, bsize: i64) -> Option<(u64, u64)> {
    if bsize <= 0 || blocks < bfree {
        return None;
    }
    let total = blocks * (bsize as u64) / BYTES_PER_MIB;
    let used = (blocks - bfree) * (bsize as u64) / BYTES_PER_MIB;
    if total > 0 {
        Some((total, used))
    } else {
        None
    }
}

pub fn memory_used_mb_from_kb(
    total: u64,
    available: u64,
    free: u64,
    buffers: u64,
    cached: u64,
) -> u64 {
    let avail = if available == 0 {
        free + buffers + cached
    } else {
        available
    };
    if total < avail {
        0
    } else {
        (total - avail) / 1024
    }
}

pub fn swap_used_mb_from_kb(total: u64, free: u64) -> u64 {
    if total < free {
        0
    } else {
        (total - free) / 1024
    }
}

pub fn should_include_net_interface(name: &str, wanted: &HashSet<String>) -> bool {
    if is_excluded_net_interface(name) {
        return false;
    }
    if wanted.is_empty() {
        return true;
    }
    for pattern in wanted {
        if pattern == name {
            return true;
        }
        if glob_match(pattern, name) {
            return true;
        }
    }
    false
}

pub fn is_excluded_net_interface(name: &str) -> bool {
    const EXCLUDED: &[&str] = &[
        "br",
        "cni",
        "docker",
        "podman",
        "flannel",
        "lo",
        "veth",
        "virbr",
        "vmbr",
        "tap",
        "fwbr",
        "fwpr",
        "tailscale",
        "tun",
        "wg",
        "wireguard",
        "ipsec",
        "gre",
        "gretap",
        "ipip",
        "sit",
        "ip6tnl",
        "zerotier",
    ];
    for prefix in EXCLUDED {
        if name.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    pattern == text
}

pub fn parse_first_uint(raw: &str) -> u64 {
    raw.split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

pub fn detect_gpu_info() -> Option<serde_json::Value> {
    use std::sync::{Arc, OnceLock, RwLock};
    static CACHE: OnceLock<Arc<RwLock<Option<serde_json::Value>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let cache = Arc::new(RwLock::new(None));
        let worker_cache = cache.clone();
        let _ = std::thread::Builder::new()
            .name("gpu-collector".into())
            .spawn(move || loop {
                let value = detect_gpu_info_uncached();
                *worker_cache.write().unwrap() = value;
                std::thread::sleep(std::time::Duration::from_secs(30));
            });
        cache
    });
    cache.read().unwrap().clone()
}

fn detect_gpu_info_uncached() -> Option<serde_json::Value> {
    if command_exists("nvidia-smi") {
        let out = command_output(
            "nvidia-smi",
            &[
                "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,clocks.sm,power.draw",
                "--format=csv,noheader,nounits",
            ],
        );
        let gpus = parse_nvidia_smi(&out, 5);
        if !gpus.is_empty() {
            return serde_json::to_value(gpus).ok();
        }

        let out = command_output(
            "nvidia-smi",
            &[
                "--query-gpu=index,name,utilization.gpu",
                "--format=csv,noheader,nounits",
            ],
        );
        let gpus = parse_nvidia_smi(&out, 1);
        if !gpus.is_empty() {
            return serde_json::to_value(gpus).ok();
        }
    }
    if command_exists("rocm-smi") {
        let out = command_output("rocm-smi", &["--showproductname", "--showuse"]);
        if !out.trim().is_empty() {
            let gpu = GpuMetric {
                name: "AMD ROCm GPU".to_string(),
                info: serde_json::json!(0),
                id: "0".to_string(),
                mem_used: None,
                mem_total: None,
                sm_clock: None,
                power: None,
            };
            return serde_json::to_value(vec![gpu]).ok();
        }
    }
    None
}

pub fn parse_nvidia_smi(out: &str, numeric_tail: usize) -> Vec<GpuMetric> {
    let mut gpus = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if numeric_tail < 1 || parts.len() < numeric_tail + 2 {
            continue;
        }
        let id = parts[0].trim().to_string();
        let name_slice = &parts[1..parts.len() - numeric_tail];
        let name = name_slice.join(",").trim().to_string();
        let tail = &parts[parts.len() - numeric_tail..];

        let mut gpu = GpuMetric {
            name,
            info: parse_nvidia_float(tail[0]),
            id,
            mem_used: None,
            mem_total: None,
            sm_clock: None,
            power: None,
        };
        if numeric_tail >= 5 {
            let u = parse_nvidia_float(tail[1]);
            if !u.is_null() {
                gpu.mem_used = Some(u);
            }
            let t = parse_nvidia_float(tail[2]);
            if !t.is_null() {
                gpu.mem_total = Some(t);
            }
            let s = parse_nvidia_float(tail[3]);
            if !s.is_null() {
                gpu.sm_clock = Some(s);
            }
            let p = parse_nvidia_float(tail[4]);
            if !p.is_null() {
                gpu.power = Some(p);
            }
        }
        gpus.push(gpu);
    }
    gpus
}

pub fn parse_nvidia_float(raw: &str) -> serde_json::Value {
    let s = raw
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if s.is_empty() || s.eq_ignore_ascii_case("N/A") || s.eq_ignore_ascii_case("NOT SUPPORTED") {
        return serde_json::Value::Null;
    }
    if let Ok(f) = s.parse::<f64>() {
        serde_json::json!(f)
    } else {
        serde_json::Value::Null
    }
}

pub fn disk_io_stats_from_counters(
    prev: DiskIOCounters,
    current: DiskIOCounters,
    elapsed_seconds: f64,
) -> DiskIOStats {
    if elapsed_seconds <= 0.0 || prev.device_count == 0 || current.device_count == 0 {
        return DiskIOStats::default();
    }
    if !prev.fingerprint.is_empty()
        && !current.fingerprint.is_empty()
        && prev.fingerprint != current.fingerprint
    {
        return DiskIOStats::default();
    }

    let read_bytes = counter_delta(prev.read_bytes, current.read_bytes);
    let write_bytes = counter_delta(prev.write_bytes, current.write_bytes);
    let read_ops = counter_delta(prev.read_ops, current.read_ops);
    let write_ops = counter_delta(prev.write_ops, current.write_ops);
    let read_time = counter_delta(prev.read_time_ms, current.read_time_ms);
    let write_time = counter_delta(prev.write_time_ms, current.write_time_ms);
    let io_ticks = counter_delta(prev.io_ticks_ms, current.io_ticks_ms);

    let total_ops = read_ops + write_ops;
    let await_ms = if total_ops > 0 {
        (read_time + write_time) as f64 / total_ops as f64
    } else {
        0.0
    };

    let device_count = current.device_count.max(1);
    let util = (io_ticks as f64) / elapsed_seconds / 10.0 / (device_count as f64);

    DiskIOStats {
        read_bps: (read_bytes as f64 / elapsed_seconds) as u64,
        write_bps: (write_bytes as f64 / elapsed_seconds) as u64,
        read_iops: round_metric(read_ops as f64 / elapsed_seconds),
        write_iops: round_metric(write_ops as f64 / elapsed_seconds),
        await_ms: round_metric(await_ms),
        util: round_metric(clamp_metric(util, 0.0, 100.0)),
    }
}

pub fn counter_delta(prev: u64, current: u64) -> u64 {
    current.saturating_sub(prev)
}

pub fn round_metric(v: f64) -> f64 {
    if v < 0.0 {
        0.0
    } else {
        (v * 100.0).round() / 100.0
    }
}

pub fn clamp_metric(v: f64, min_val: f64, max_val: f64) -> f64 {
    v.clamp(min_val, max_val)
}

pub fn sample_metrics_to_map(metrics: &Metrics) -> serde_json::Value {
    let mut value = serde_json::to_value(metrics).expect("Metrics serialization cannot fail");
    // Keep every time-varying metric in history. Host identity belongs to the
    // report's latest snapshot and need not be repeated in every history point.
    if let Some(map) = value.as_object_mut() {
        for key in [
            "os",
            "arch",
            "kernel_version",
            "cpu_info",
            "cpu_cores",
            "boot_time",
            "ip_v4",
            "ip_v6",
        ] {
            map.remove(key);
        }
    }
    value
}

pub fn native_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        "mips" => "mips",
        "mipsel" => "mipsle",
        "mips64" => "mips64",
        "mips64el" => "mips64le",
        "loongarch64" => "loong64",
        other => other,
    }
}
