use crate::metrics::common::*;
use crate::types::*;
use std::ffi::{CStr, CString};
use std::mem::{size_of, MaybeUninit};
use std::sync::{LazyLock, Mutex, OnceLock};

#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

static PREVIOUS_CPU_TIMES: LazyLock<Mutex<Option<CpuTimes>>> = LazyLock::new(|| Mutex::new(None));
#[allow(deprecated)]
static HOST_PORT: LazyLock<libc::host_t> = LazyLock::new(|| unsafe { libc::mach_host_self() });

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_listpids(
        kind: u32,
        type_info: u32,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
}

#[allow(deprecated)]
pub fn read_cpu_percent() -> (f64, bool) {
    let mut times: libc::host_cpu_load_info_data_t = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let status = unsafe {
        libc::host_statistics(
            *HOST_PORT,
            libc::HOST_CPU_LOAD_INFO,
            (&mut times as *mut libc::host_cpu_load_info_data_t).cast(),
            &mut count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return (0.0, false);
    }

    let idle = times.cpu_ticks[libc::CPU_STATE_IDLE as usize] as u64;
    let total = times.cpu_ticks.iter().map(|ticks| *ticks as u64).sum();
    let current = CpuTimes { total, idle };
    let mut previous = PREVIOUS_CPU_TIMES.lock().unwrap();
    previous
        .replace(current)
        .and_then(|prior| {
            let total_delta = current.total.saturating_sub(prior.total);
            let idle_delta = current.idle.saturating_sub(prior.idle);
            (total_delta > 0 && idle_delta <= total_delta)
                .then(|| ((total_delta - idle_delta) as f64 / total_delta as f64) * 100.0)
        })
        .map(|value| (value, true))
        .unwrap_or((0.0, false))
}

pub fn collect_basic_stats() -> BasicStats {
    let (memory, _) = read_memory_stats();
    let (disk_total_mb, disk_used_mb) = darwin_disk_usage("/");
    let cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as usize;
    static IDENTITY: OnceLock<(String, String, String)> = OnceLock::new();
    let identity = IDENTITY.get_or_init(|| {
        let kernel = sysctl_string("kern.osrelease").unwrap_or_else(|| "Darwin".to_string());
        let product_version = sysctl_string("kern.osproductversion").unwrap_or_default();
        let os_name = if product_version.is_empty() {
            "macOS".to_string()
        } else {
            format!("macOS {}", product_version)
        };
        let cpu_info = sysctl_string("machdep.cpu.brand_string")
            .or_else(|| sysctl_string("hw.model"))
            .unwrap_or_else(|| native_arch().to_string());

        (os_name, kernel, cpu_info)
    });

    BasicStats {
        mem_total_mb: memory.mem_total_mb,
        mem_used_mb: memory.mem_used_mb,
        swap_total_mb: memory.swap_total_mb,
        swap_used_mb: memory.swap_used_mb,
        disk_total_mb,
        disk_used_mb,
        disk_devices: Vec::new(),
        load_avg: darwin_load_avg(),
        boot_time_ms: darwin_boot_time_ms(),
        os_name: identity.0.clone(),
        arch: native_arch().to_string(),
        kernel: identity.1.clone(),
        cpu_info: identity.2.clone(),
        cpu_cores: cores,
        gpu_info: darwin_gpu_info(),
        processes: darwin_process_count(),
        tcp_conn: darwin_conn_count("tcp"),
        udp_conn: darwin_conn_count("udp"),
    }
}

#[allow(deprecated)]
pub fn read_memory_stats() -> (MemoryStats, bool) {
    let total_bytes = match sysctl_value::<u64>("hw.memsize") {
        Some(total) => total,
        None => return (MemoryStats::default(), false),
    };
    let mut vm: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let status = unsafe {
        libc::host_statistics64(
            *HOST_PORT,
            libc::HOST_VM_INFO64,
            (&mut vm as *mut libc::vm_statistics64_data_t).cast(),
            &mut count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return (MemoryStats::default(), false);
    }

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let available_pages = vm.free_count as u64 + vm.speculative_count as u64;
    let total_mb = total_bytes / BYTES_PER_MIB;
    let available_mb = available_pages.saturating_mul(page_size) / BYTES_PER_MIB;
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
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut length = 0usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut buffer = vec![0u8; length];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let mut total = NetBytes::default();
    let mut matched = 0usize;
    let mut offset = 0usize;
    while offset + size_of::<libc::if_msghdr2>() <= length {
        let message_len = u16::from_ne_bytes([buffer[offset], buffer[offset + 1]]) as usize;
        if message_len < size_of::<libc::if_msghdr2>() || offset + message_len > length {
            break;
        }
        if buffer[offset + 3] == libc::RTM_IFINFO2 as u8 {
            let header = unsafe {
                std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<libc::if_msghdr2>())
            };
            let mut name = [0 as libc::c_char; libc::IFNAMSIZ];
            if !unsafe { libc::if_indextoname(header.ifm_index as u32, name.as_mut_ptr()) }
                .is_null()
            {
                let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy();
                if should_include_net_interface(&name, &wanted) {
                    total.rx = total.rx.saturating_add(header.ifm_data.ifi_ibytes);
                    total.tx = total.tx.saturating_add(header.ifm_data.ifi_obytes);
                    matched += 1;
                }
            }
        }
        offset += message_len;
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

fn darwin_disk_usage(mount: &str) -> (u64, u64) {
    let mount = match CString::new(mount) {
        Ok(mount) => mount,
        Err(_) => return (0, 0),
    };
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(mount.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return (0, 0);
    }
    let stat = unsafe { stat.assume_init() };
    let Some((total, used)) =
        disk_usage_mb_from_blocks(stat.f_blocks, stat.f_bfree, stat.f_bsize as i64)
    else {
        return (0, 0);
    };
    (total, used)
}

fn darwin_load_avg() -> String {
    let mut load = [0.0f64; 3];
    if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } == 3 {
        format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2])
    } else {
        "0 0 0".to_string()
    }
}

fn darwin_boot_time_ms() -> i64 {
    sysctl_value::<libc::timeval>("kern.boottime")
        .map(|time| time.tv_sec.saturating_mul(1000) + (time.tv_usec / 1000) as i64)
        .unwrap_or(0)
}

fn darwin_process_count() -> usize {
    const PROC_ALL_PIDS: u32 = 1;
    let mut pids = vec![0i32; 65_536];
    let bytes = unsafe {
        proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        0
    } else {
        (bytes as usize / size_of::<i32>()).min(pids.len())
    }
}

fn darwin_gpu_info() -> Option<serde_json::Value> {
    // The generic host metrics above are read from macOS system APIs. Keep the
    // existing optional vendor tool path for GPU telemetry only.
    detect_gpu_info()
}

fn darwin_conn_count(kind: &str) -> usize {
    let key = if kind == "tcp" {
        "net.inet.tcp.pcbcount"
    } else {
        "net.inet.udp.pcbcount"
    };
    sysctl_value::<libc::c_int>(key).unwrap_or(0).max(0) as usize
}

fn sysctl_value<T: Copy>(name: &str) -> Option<T> {
    let name = CString::new(name).ok()?;
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>();
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size != size_of::<T>() {
        None
    } else {
        Some(unsafe { value.assume_init() })
    }
}

fn sysctl_string(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let mut size = 0usize;
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
    {
        return None;
    }
    let mut bytes = vec![0u8; size];
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let value = String::from_utf8_lossy(&bytes[..size.min(bytes.len())])
        .trim_end_matches('\0')
        .trim()
        .to_string();
    (!value.is_empty()).then_some(value)
}
