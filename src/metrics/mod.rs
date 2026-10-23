pub mod common;
pub use common::*;
#[cfg(target_os = "macos")]
pub mod darwin;
#[cfg(target_os = "freebsd")]
pub mod freebsd;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod other;
#[cfg(target_os = "windows")]
pub mod windows;

use crate::types::*;

pub fn read_cpu_percent() -> (f64, bool) {
    #[cfg(target_os = "linux")]
    return linux::read_cpu_percent();
    #[cfg(target_os = "macos")]
    return darwin::read_cpu_percent();
    #[cfg(target_os = "windows")]
    return windows::read_cpu_percent();
    #[cfg(target_os = "freebsd")]
    return freebsd::read_cpu_percent();
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    )))]
    return other::read_cpu_percent();
}

pub fn collect_basic_stats() -> BasicStats {
    #[cfg(target_os = "linux")]
    return linux::collect_basic_stats();
    #[cfg(target_os = "macos")]
    return darwin::collect_basic_stats();
    #[cfg(target_os = "windows")]
    return windows::collect_basic_stats();
    #[cfg(target_os = "freebsd")]
    return freebsd::collect_basic_stats();
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    )))]
    return other::collect_basic_stats();
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    #[cfg(target_os = "linux")]
    return linux::read_memory_stats();
    #[cfg(target_os = "macos")]
    return darwin::read_memory_stats();
    #[cfg(target_os = "windows")]
    return windows::read_memory_stats();
    #[cfg(target_os = "freebsd")]
    return freebsd::read_memory_stats();
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    )))]
    return other::read_memory_stats();
}

pub fn read_net_bytes(ifaces: &str) -> Result<NetBytes, String> {
    #[cfg(target_os = "linux")]
    return linux::read_net_bytes(ifaces);
    #[cfg(target_os = "macos")]
    return darwin::read_net_bytes(ifaces);
    #[cfg(target_os = "windows")]
    return windows::read_net_bytes(ifaces);
    #[cfg(target_os = "freebsd")]
    return freebsd::read_net_bytes(ifaces);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    )))]
    return other::read_net_bytes(ifaces);
}

pub fn read_disk_io_counters(devices: &[DiskDeviceRef]) -> DiskIOCounters {
    #[cfg(target_os = "linux")]
    return linux::read_disk_io_counters(devices);
    #[cfg(target_os = "macos")]
    return darwin::read_disk_io_counters(devices);
    #[cfg(target_os = "windows")]
    return windows::read_disk_io_counters(devices);
    #[cfg(target_os = "freebsd")]
    return freebsd::read_disk_io_counters(devices);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    )))]
    return other::read_disk_io_counters(devices);
}
