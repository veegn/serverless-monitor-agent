use crate::metrics::common::*;
use crate::types::*;
use std::mem::{size_of, zeroed};
use std::sync::{LazyLock, Mutex, OnceLock};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetExtendedTcpTable, GetExtendedUdpTable, GetIfTable2, MIB_IF_ROW2,
    MIB_IF_TABLE2, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCP_STATE_ESTAB,
    MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemInfo, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
};
use windows_sys::Win32::System::Threading::GetSystemTimes;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

static PREVIOUS_CPU_TIMES: LazyLock<Mutex<Option<CpuTimes>>> = LazyLock::new(|| Mutex::new(None));

pub fn read_cpu_percent() -> (f64, bool) {
    let mut idle: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return (0.0, false);
    }

    let idle = filetime_value(idle);
    let kernel = filetime_value(kernel);
    let user = filetime_value(user);
    let current = CpuTimes {
        total: kernel.saturating_add(user),
        idle,
    };
    let mut previous = PREVIOUS_CPU_TIMES.lock().unwrap();
    let result = previous.replace(current).and_then(|prior| {
        let total_delta = current.total.saturating_sub(prior.total);
        let idle_delta = current.idle.saturating_sub(prior.idle);
        (total_delta > 0 && idle_delta <= total_delta)
            .then(|| ((total_delta - idle_delta) as f64 / total_delta as f64) * 100.0)
    });
    result.map(|value| (value, true)).unwrap_or((0.0, false))
}

pub fn collect_basic_stats() -> BasicStats {
    let (memory, _) = read_memory_stats();
    let (disk_total_mb, disk_used_mb) = windows_disk_usage();
    let mut system_info: SYSTEM_INFO = unsafe { zeroed() };
    unsafe { GetSystemInfo(&mut system_info) };

    static IDENTITY: OnceLock<(String, String, String)> = OnceLock::new();
    let identity = IDENTITY.get_or_init(|| {
        (
            registry_string(
                "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
                "ProductName",
            )
            .unwrap_or_else(|| "Microsoft Windows".to_string()),
            registry_string(
                "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
                "CurrentBuildNumber",
            )
            .unwrap_or_else(|| "Windows".to_string()),
            registry_string(
                "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
                "ProcessorNameString",
            )
            .unwrap_or_else(|| native_arch().to_string()),
        )
    });
    BasicStats {
        mem_total_mb: memory.mem_total_mb,
        mem_used_mb: memory.mem_used_mb,
        swap_total_mb: memory.swap_total_mb,
        swap_used_mb: memory.swap_used_mb,
        disk_total_mb,
        disk_used_mb,
        disk_devices: Vec::new(),
        load_avg: "0 0 0".to_string(),
        boot_time_ms: windows_boot_time_ms(),
        os_name: identity.0.clone(),
        arch: native_arch().to_string(),
        kernel: identity.1.clone(),
        cpu_info: identity.2.clone(),
        cpu_cores: system_info.dwNumberOfProcessors as usize,
        gpu_info: windows_gpu_info(),
        processes: windows_process_count(),
        tcp_conn: windows_tcp_connection_count(),
        udp_conn: windows_udp_endpoint_count(),
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    let mut status: MEMORYSTATUSEX = unsafe { zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return (MemoryStats::default(), false);
    }

    let total_mb = status.ullTotalPhys / BYTES_PER_MIB;
    let available_mb = status.ullAvailPhys / BYTES_PER_MIB;
    (
        MemoryStats {
            mem_total_mb: total_mb,
            mem_used_mb: total_mb.saturating_sub(available_mb),
            swap_total_mb: 0,
            swap_used_mb: 0,
        },
        total_mb > 0,
    )
}

pub fn read_net_bytes(ifaces: &str) -> Result<NetBytes, String> {
    let wanted = crate::storage::config::split_interface_set(ifaces);
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    let status = unsafe { GetIfTable2(&mut table) };
    if status != NO_ERROR || table.is_null() {
        return Err(format!("GetIfTable2 failed with status {}", status));
    }

    let mut total = NetBytes::default();
    let mut matched = 0usize;
    unsafe {
        let count = (*table).NumEntries as usize;
        let rows = std::ptr::addr_of!((*table).Table) as *const MIB_IF_ROW2;
        for index in 0..count {
            let row = &*rows.add(index);
            let name = String::from_utf16_lossy(
                &row.Alias[..row
                    .Alias
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(row.Alias.len())],
            );
            if !should_include_net_interface(&name, &wanted) {
                continue;
            }
            total.rx = total.rx.saturating_add(row.InOctets);
            total.tx = total.tx.saturating_add(row.OutOctets);
            matched += 1;
        }
        FreeMibTable(table.cast());
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

fn windows_disk_usage() -> (u64, u64) {
    let drives = unsafe { GetLogicalDrives() };
    let mut total_bytes = 0u64;
    let mut used_bytes = 0u64;
    for index in 0..26 {
        if drives & (1 << index) == 0 {
            continue;
        }
        let root = [b'A' as u16 + index as u16, b':' as u16, b'\\' as u16, 0];
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut total = 0u64;
        let mut free = 0u64;
        if unsafe {
            GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut total, &mut free)
        } != 0
        {
            total_bytes = total_bytes.saturating_add(total);
            used_bytes = used_bytes.saturating_add(total.saturating_sub(free));
        }
    }
    (total_bytes / BYTES_PER_MIB, used_bytes / BYTES_PER_MIB)
}

fn windows_boot_time_ms() -> i64 {
    let uptime_ms = unsafe { GetTickCount64() } as i64;
    chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(uptime_ms)
}

fn windows_process_count() -> usize {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == 0 || snapshot == -1isize {
        return 0;
    }

    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut count = 0usize;
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                count += 1;
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    count
}

fn windows_tcp_connection_count() -> usize {
    tcp_table_count(AF_INET as u32) + tcp_table_count(AF_INET6 as u32)
}

fn tcp_table_count(address_family: u32) -> usize {
    let mut size = 0u32;
    let first = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            address_family,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
        return 0;
    }
    let mut table = vec![0u8; size as usize];
    if table.is_empty()
        || unsafe {
            GetExtendedTcpTable(
                table.as_mut_ptr().cast(),
                &mut size,
                0,
                address_family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        } != NO_ERROR
    {
        return 0;
    }

    let entries = u32::from_ne_bytes(table[..4].try_into().unwrap()) as usize;
    if address_family == AF_INET as u32 {
        let row_size = size_of::<MIB_TCPROW_OWNER_PID>();
        (0..entries.min(table.len().saturating_sub(4) / row_size))
            .filter(|index| {
                let ptr = unsafe {
                    table
                        .as_ptr()
                        .add(4 + index * row_size)
                        .cast::<MIB_TCPROW_OWNER_PID>()
                };
                (unsafe { std::ptr::read_unaligned(ptr).dwState }) == MIB_TCP_STATE_ESTAB as u32
            })
            .count()
    } else {
        let row_size = size_of::<MIB_TCP6ROW_OWNER_PID>();
        (0..entries.min(table.len().saturating_sub(4) / row_size))
            .filter(|index| {
                let ptr = unsafe {
                    table
                        .as_ptr()
                        .add(4 + index * row_size)
                        .cast::<MIB_TCP6ROW_OWNER_PID>()
                };
                (unsafe { std::ptr::read_unaligned(ptr).dwState }) == MIB_TCP_STATE_ESTAB as u32
            })
            .count()
    }
}

fn windows_udp_endpoint_count() -> usize {
    udp_table_count(AF_INET as u32) + udp_table_count(AF_INET6 as u32)
}

fn udp_table_count(address_family: u32) -> usize {
    let mut size = 0u32;
    let first = unsafe {
        GetExtendedUdpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            address_family,
            UDP_TABLE_OWNER_PID,
            0,
        )
    };
    if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
        return 0;
    }
    let mut table = vec![0u8; size as usize];
    if table.is_empty()
        || unsafe {
            GetExtendedUdpTable(
                table.as_mut_ptr().cast(),
                &mut size,
                0,
                address_family,
                UDP_TABLE_OWNER_PID,
                0,
            )
        } != NO_ERROR
    {
        return 0;
    }
    let entries = u32::from_ne_bytes(table[..4].try_into().unwrap()) as usize;
    let row_size = if address_family == AF_INET as u32 {
        size_of::<MIB_UDPROW_OWNER_PID>()
    } else {
        size_of::<MIB_UDP6ROW_OWNER_PID>()
    };
    entries.min(table.len().saturating_sub(4) / row_size)
}

fn windows_gpu_info() -> Option<serde_json::Value> {
    // Vendor-provided GPU tools are optional; all built-in host metrics use
    // Windows system APIs directly.
    detect_gpu_info()
}

fn registry_string(subkey: &str, value: &str) -> Option<String> {
    let subkey = wide_nul(subkey);
    let value = wide_nul(value);
    let mut data = vec![0u16; 512];
    let mut bytes = (data.len() * size_of::<u16>()) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != NO_ERROR {
        return None;
    }
    let units = (bytes as usize / size_of::<u16>()).min(data.len());
    let end = data[..units]
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units);
    let result = String::from_utf16_lossy(&data[..end]).trim().to_string();
    (!result.is_empty()).then_some(result)
}

fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn filetime_value(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}
