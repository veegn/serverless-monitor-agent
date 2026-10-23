use crate::command::{command_line_count, command_output};
use crate::metrics::common::*;
use crate::types::*;
use std::collections::HashSet;

pub fn read_cpu_percent() -> (f64, bool) {
    let out = command_output("top", &["-b", "-d", "1"]);
    for line in out.lines() {
        if line.contains("CPU:") {
            for part in line.split(',') {
                if part.contains("idle") {
                    if let Some(idle) = parse_percent(part) {
                        return (100.0 - idle, true);
                    }
                }
            }
        }
    }
    (0.0, false)
}

pub fn collect_basic_stats() -> BasicStats {
    let (total, used) = freebsd_memory_mb();
    let (disk_total, disk_used) = freebsd_disk_usage();
    BasicStats {
        mem_total_mb: total,
        mem_used_mb: used,
        swap_total_mb: 0,
        swap_used_mb: 0,
        disk_total_mb: disk_total,
        disk_used_mb: disk_used,
        disk_devices: Vec::new(),
        load_avg: freebsd_load_avg(),
        boot_time_ms: freebsd_boot_time_ms(),
        os_name: "FreeBSD".to_string(),
        arch: native_arch().to_string(),
        kernel: command_output("uname", &["-r"]),
        cpu_info: command_output("sysctl", &["-n", "hw.model"]),
        cpu_cores: command_output("sysctl", &["-n", "hw.ncpu"]).trim().parse().unwrap_or(1),
        gpu_info: detect_gpu_info(),
        processes: command_line_count("ps", &["-ax"]).saturating_sub(1),
        tcp_conn: freebsd_conn_count("tcp"),
        udp_conn: freebsd_conn_count("udp"),
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    let (total, used) = freebsd_memory_mb();
    if total == 0 {
        (MemoryStats::default(), false)
    } else {
        (
            MemoryStats {
                mem_total_mb: total,
                mem_used_mb: used,
                swap_total_mb: 0,
                swap_used_mb: 0,
            },
            true,
        )
    }
}

pub fn read_net_bytes(ifaces: &str) -> Result<NetBytes, String> {
    let wanted: HashSet<String> = crate::config::split_interface_set(ifaces);
    let out = command_output("netstat", &["-b", "-i", "-n"]);
    let mut total = NetBytes::default();
    let mut matched = 0;
    let mut seen = HashSet::new();

    for line in out.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        let name = fields[0].trim_end_matches('*');
        if !should_include_net_interface(name, &wanted) {
            continue;
        }
        if seen.contains(name) {
            continue;
        }
        let ibytes: u64 = fields[6].parse().unwrap_or(0);
        let obytes: u64 = fields[9].parse().unwrap_or(0);
        total.rx += ibytes;
        total.tx += obytes;
        matched += 1;
        seen.insert(name.to_string());
    }

    if matched == 0 {
        Err("no matching network interfaces".to_string())
    } else {
        Ok(total)
    }
}

pub fn read_disk_io_counters(_devices: &[DiskDeviceRef]) -> DiskIOCounters {
    DiskIOCounters::default()
}

fn parse_percent(s: &str) -> Option<f64> {
    if let Some((num_part, _)) = s.trim().split_once('%') {
        num_part.split_whitespace().last()?.parse::<f64>().ok()
    } else {
        None
    }
}

fn freebsd_memory_mb() -> (u64, u64) {
    let total_bytes: u64 = command_output("sysctl", &["-n", "hw.physmem"]).trim().parse().unwrap_or(0);
    let total_mb = total_bytes / BYTES_PER_MIB;
    let page_size: u64 = command_output("sysctl", &["-n", "hw.pagesize"]).trim().parse().unwrap_or(4096);
    let free_pages: u64 = command_output("sysctl", &["-n", "vm.stats.vm.v_free_count"]).trim().parse().unwrap_or(0);
    let free_mb = (free_pages * page_size) / BYTES_PER_MIB;
    let used_mb = total_mb.saturating_sub(free_mb);
    (total_mb, used_mb)
}

fn freebsd_disk_usage() -> (u64, u64) {
    let out = command_output("df", &["-k"]);
    let mut total_kb = 0;
    let mut used_kb = 0;
    for line in out.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 6 && fields[0].starts_with("/dev/") {
            let t: u64 = fields[1].parse().unwrap_or(0);
            let u: u64 = fields[2].parse().unwrap_or(0);
            total_kb += t;
            used_kb += u;
        }
    }
    (total_kb / 1024, used_kb / 1024)
}

fn freebsd_load_avg() -> String {
    let out = command_output("sysctl", &["-n", "vm.loadavg"]);
    let trimmed = out.trim().trim_start_matches('{').trim_end_matches('}').trim();
    if !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        "0 0 0".to_string()
    }
}

fn freebsd_boot_time_ms() -> i64 {
    let out = command_output("sysctl", &["-n", "kern.boottime"]);
    if let Some((_, rest)) = out.split_once("sec =") {
        if let Some(sec_str) = rest.split(',').next() {
            if let Ok(sec) = sec_str.trim().parse::<i64>() {
                return sec * 1000;
            }
        }
    }
    0
}

fn freebsd_conn_count(kind: &str) -> usize {
    let out = command_output("netstat", &["-an"]);
    let mut count = 0;
    for line in out.lines() {
        let line_lower = line.to_ascii_lowercase();
        if kind == "tcp" {
            if line_lower.starts_with("tcp") && line_lower.contains("established") {
                count += 1;
            }
        } else if kind == "udp" && line_lower.starts_with("udp") {
            count += 1;
        }
    }
    count
}
