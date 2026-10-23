use crate::command::{command_line_count, command_output};
use crate::metrics::common::*;
use crate::types::*;
use std::collections::HashSet;

pub fn read_cpu_percent() -> (f64, bool) {
    if let Some(usage) = darwin_top_cpu_percent() {
        return (usage, true);
    }
    if let Some(usage) = darwin_ps_cpu_percent() {
        return (usage, true);
    }
    (0.0, false)
}

pub fn collect_basic_stats() -> BasicStats {
    let (total, used) = darwin_memory_mb();
    let (disk_total, disk_used) = darwin_disk_usage("/");
    BasicStats {
        mem_total_mb: total,
        mem_used_mb: used,
        swap_total_mb: 0,
        swap_used_mb: 0,
        disk_total_mb: disk_total,
        disk_used_mb: disk_used,
        disk_devices: Vec::new(),
        load_avg: darwin_load_avg(),
        boot_time_ms: darwin_boot_time_ms(),
        os_name: darwin_os_name(),
        arch: native_arch().to_string(),
        kernel: command_output("uname", &["-r"]),
        cpu_info: darwin_cpu_info(),
        cpu_cores: num_cpus(),
        gpu_info: darwin_gpu_info(),
        processes: command_line_count("ps", &["-e"]).saturating_sub(1),
        tcp_conn: darwin_conn_count("tcp"),
        udp_conn: darwin_conn_count("udp"),
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    let (total, used) = darwin_memory_mb();
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
    let out = command_output("netstat", &["-b", "-i"]);
    let mut total = NetBytes::default();
    let mut matched = 0;
    let mut seen = HashSet::new();

    for line in out.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        let name = fields[0];
        if !should_include_net_interface(name, &wanted) {
            continue;
        }
        if seen.contains(name) {
            continue;
        }
        // Name Mtu Network Address Ipkts Ierrs Ibytes Opkts Oerrs Obytes
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

fn darwin_top_cpu_percent() -> Option<f64> {
    let out = command_output("top", &["-l", "1", "-n", "0", "-s", "0"]);
    for line in out.lines() {
        if line.contains("CPU usage:") {
            for part in line.split(',') {
                if part.contains("idle") {
                    if let Some(idle) = parse_percent_before_mark(part) {
                        return Some(100.0 - idle);
                    }
                }
            }
        }
    }
    None
}

fn darwin_ps_cpu_percent() -> Option<f64> {
    let out = command_output("ps", &["-A", "-o", "%cpu="]);
    let mut total = 0.0;
    let mut found = false;
    for field in out.split_whitespace() {
        if let Ok(v) = field.parse::<f64>() {
            total += v;
            found = true;
        }
    }
    if found {
        Some(total)
    } else {
        None
    }
}

fn parse_percent_before_mark(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some((num_part, _)) = s.split_once('%') {
        num_part.split_whitespace().last()?.parse::<f64>().ok()
    } else {
        None
    }
}

fn darwin_memory_mb() -> (u64, u64) {
    let memsize_str = command_output("sysctl", &["-n", "hw.memsize"]);
    let total_bytes: u64 = memsize_str.trim().parse().unwrap_or(0);
    let total_mb = total_bytes / BYTES_PER_MIB;

    // vm_stat for free/inactive
    let vm_stat = command_output("vm_stat", &[]);
    let page_size_str = command_output("pagesize", &[]);
    let page_size: u64 = page_size_str.trim().parse().unwrap_or(4096);

    let mut free_pages: u64 = 0;
    let mut speculative_pages: u64 = 0;

    for line in vm_stat.lines() {
        if line.starts_with("Pages free:") {
            if let Some((_, val)) = line.split_once(':') {
                free_pages = val.trim().trim_end_matches('.').parse().unwrap_or(0);
            }
        } else if line.starts_with("Pages speculative:") {
            if let Some((_, val)) = line.split_once(':') {
                speculative_pages = val.trim().trim_end_matches('.').parse().unwrap_or(0);
            }
        }
    }

    let free_bytes = (free_pages + speculative_pages) * page_size;
    let free_mb = free_bytes / BYTES_PER_MIB;
    let used_mb = total_mb.saturating_sub(free_mb);
    (total_mb, used_mb)
}

fn darwin_disk_usage(mount: &str) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        if let Ok(c_mount) = CString::new(mount) {
            let mut stat: MaybeUninit<libc::statfs> = MaybeUninit::uninit();
            let res = unsafe { libc::statfs(c_mount.as_ptr(), stat.as_mut_ptr()) };
            if res == 0 {
                let stat = unsafe { stat.assume_init() };
                let bsize = stat.f_bsize as i64;
                let blocks = stat.f_blocks;
                let bfree = stat.f_bfree;
                if let Some((total, used)) = disk_usage_mb_from_blocks(blocks, bfree, bsize) {
                    return (total, used);
                }
            }
        }
    }
    (0, 0)
}

fn darwin_load_avg() -> String {
    #[cfg(unix)]
    {
        let mut load: [f64; 3] = [0.0; 3];
        let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
        if n == 3 {
            return format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2]);
        }
    }
    "0 0 0".to_string()
}

fn darwin_boot_time_ms() -> i64 {
    #[cfg(unix)]
    {
        use std::mem::MaybeUninit;
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let mut boottime: MaybeUninit<libc::timeval> = MaybeUninit::uninit();
        let mut size = std::mem::size_of::<libc::timeval>();
        let res = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                boottime.as_mut_ptr() as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if res == 0 {
            let tv = unsafe { boottime.assume_init() };
            return (tv.tv_sec as i64) * 1000 + (tv.tv_usec as i64) / 1000;
        }
    }
    0
}

fn darwin_os_name() -> String {
    let prod = command_output("sw_vers", &["-productName"]);
    let ver = command_output("sw_vers", &["-productVersion"]);
    if prod.is_empty() && ver.is_empty() {
        "macOS".to_string()
    } else {
        format!("{} {}", prod, ver).trim().to_string()
    }
}

fn darwin_cpu_info() -> String {
    let brand = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    if !brand.is_empty() {
        brand
    } else {
        native_arch().to_string()
    }
}

fn num_cpus() -> usize {
    #[cfg(unix)]
    {
        unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) as usize }
    }
    #[cfg(not(unix))]
    {
        1
    }
}

fn darwin_gpu_info() -> Option<serde_json::Value> {
    if let Some(gpu) = detect_gpu_info() {
        return Some(gpu);
    }
    let out = command_output("system_profiler", &["SPDisplaysDataType", "-json"]);
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&out) {
        if let Some(items) = val.get("SPDisplaysDataType").and_then(|v| v.as_array()) {
            let mut gpus = Vec::new();
            for (idx, item) in items.iter().enumerate() {
                let name = item.get("sppci_model").and_then(|v| v.as_str()).unwrap_or("Apple GPU");
                gpus.push(serde_json::json!({
                    "name": name,
                    "info": serde_json::Value::Null,
                    "id": idx.to_string(),
                }));
            }
            if !gpus.is_empty() {
                return Some(serde_json::Value::Array(gpus));
            }
        }
    }
    None
}

fn darwin_conn_count(kind: &str) -> usize {
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
