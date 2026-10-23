use crate::util::command::command_output_checked;
use crate::metrics::common::*;
use crate::types::*;
use std::collections::HashSet;

pub fn read_cpu_percent() -> (f64, bool) {
    let script = "(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average";
    if let Ok(out) = windows_powershell_output_checked(script) {
        if let Ok(v) = out.trim().parse::<f64>() {
            return (v, true);
        }
    }
    (0.0, false)
}

pub fn collect_basic_stats() -> BasicStats {
    let (total, used) = windows_memory_mb();
    let (disk_total, disk_used) = windows_disk_usage();
    BasicStats {
        mem_total_mb: total,
        mem_used_mb: used,
        swap_total_mb: 0,
        swap_used_mb: 0,
        disk_total_mb: disk_total,
        disk_used_mb: disk_used,
        disk_devices: Vec::new(),
        load_avg: "0 0 0".to_string(),
        boot_time_ms: windows_boot_time_ms(),
        os_name: windows_os_name(),
        arch: native_arch().to_string(),
        kernel: windows_kernel_version(),
        cpu_info: windows_cpu_info(),
        cpu_cores: windows_cpu_cores(),
        gpu_info: windows_gpu_info(),
        processes: windows_process_count(),
        tcp_conn: windows_conn_count("TCP"),
        udp_conn: windows_conn_count("UDP"),
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    let (total, used) = windows_memory_mb();
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
    let script = "Get-NetAdapterStatistics | ForEach-Object { \"$($_.Name),$($_.ReceivedBytes),$($_.SentBytes)\" }";
    let out = windows_powershell_output_checked(script)?;
    let wanted = crate::storage::config::split_interface_set(ifaces);
    let mut total = NetBytes::default();
    let mut matched = 0;

    for line in out.lines() {
        let parts: Vec<&str> = line.trim().split(',').collect();
        if parts.len() != 3 {
            continue;
        }
        let name = parts[0].trim();
        if !should_include_net_interface(name, &wanted) {
            continue;
        }
        let rx: u64 = parts[1]
            .trim()
            .parse()
            .map_err(|_| format!("invalid network counters for {}", name))?;
        let tx: u64 = parts[2]
            .trim()
            .parse()
            .map_err(|_| format!("invalid network counters for {}", name))?;
        total.rx += rx;
        total.tx += tx;
        matched += 1;
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

fn windows_powershell_output_checked(script: &str) -> Result<String, String> {
    let prefix = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8; ";
    command_output_checked(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!("{}{}", prefix, script),
        ],
    )
}

fn windows_memory_mb() -> (u64, u64) {
    if let Ok(out) = windows_powershell_output_checked(
        "$o=Get-CimInstance Win32_OperatingSystem; \"$($o.TotalVisibleMemorySize),$($o.FreePhysicalMemory)\"",
    ) {
        let parts: Vec<&str> = out.trim().split(',').collect();
        if parts.len() == 2 {
            let total_kb = parse_first_uint(parts[0]);
            let free_kb = parse_first_uint(parts[1]);
            let total_mb = total_kb / 1024;
            let used_mb = if total_kb >= free_kb {
                (total_kb - free_kb) / 1024
            } else {
                0
            };
            return (total_mb, used_mb);
        }
    }
    (0, 0)
}

fn windows_disk_usage() -> (u64, u64) {
    let script = "Get-CimInstance Win32_LogicalDisk -Filter \"DriveType=3\" | ForEach-Object { \"$($_.Size),$($_.FreeSpace)\" }";
    if let Ok(out) = windows_powershell_output_checked(script) {
        let mut total = 0;
        let mut used = 0;
        for line in out.lines() {
            let parts: Vec<&str> = line.trim().split(',').collect();
            if parts.len() == 2 {
                let size = parse_first_uint(parts[0]) / BYTES_PER_MIB;
                let free = parse_first_uint(parts[1]) / BYTES_PER_MIB;
                if size >= free {
                    total += size;
                    used += size - free;
                }
            }
        }
        return (total, used);
    }
    (0, 0)
}

fn windows_boot_time_ms() -> i64 {
    let script = "([DateTimeOffset](Get-CimInstance Win32_OperatingSystem).LastBootUpTime).ToUnixTimeMilliseconds()";
    if let Ok(out) = windows_powershell_output_checked(script) {
        if let Ok(ms) = out.trim().parse::<i64>() {
            return ms;
        }
    }
    chrono::Utc::now().timestamp_millis()
}

fn windows_os_name() -> String {
    let script = "(Get-CimInstance Win32_OperatingSystem).Caption";
    windows_powershell_output_checked(script).unwrap_or_else(|_| "Microsoft Windows".to_string())
}

fn windows_kernel_version() -> String {
    let script = "(Get-CimInstance Win32_OperatingSystem).Version";
    windows_powershell_output_checked(script).unwrap_or_else(|_| "Windows".to_string())
}

fn windows_cpu_info() -> String {
    let script = "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)";
    windows_powershell_output_checked(script).unwrap_or_else(|_| native_arch().to_string())
}

fn windows_cpu_cores() -> usize {
    let script = "(Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum";
    if let Ok(out) = windows_powershell_output_checked(script) {
        if let Ok(n) = out.trim().parse::<usize>() {
            return n;
        }
    }
    1
}

fn windows_process_count() -> usize {
    let script = "(Get-Process).Count";
    if let Ok(out) = windows_powershell_output_checked(script) {
        if let Ok(n) = out.trim().parse::<usize>() {
            return n;
        }
    }
    0
}

fn windows_conn_count(kind: &str) -> usize {
    let cmdlet = if kind == "UDP" {
        "Get-NetUDPEndpoint"
    } else {
        "Get-NetTCPConnection"
    };
    let script = format!("({}).Count", cmdlet);
    if let Ok(out) = windows_powershell_output_checked(&script) {
        if let Ok(n) = out.trim().parse::<usize>() {
            return n;
        }
    }
    0
}

fn windows_gpu_info() -> Option<serde_json::Value> {
    if let Some(gpu) = detect_gpu_info() {
        return Some(gpu);
    }
    let script = "Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }";
    if let Ok(out) = windows_powershell_output_checked(script) {
        let mut gpus = Vec::new();
        for (idx, line) in out.lines().enumerate() {
            let name = line.trim();
            if !name.is_empty() {
                gpus.push(serde_json::json!({
                    "name": name,
                    "info": serde_json::Value::Null,
                    "id": idx.to_string(),
                }));
            }
        }
        if !gpus.is_empty() {
            return Some(serde_json::Value::Array(gpus));
        }
    }
    None
}
