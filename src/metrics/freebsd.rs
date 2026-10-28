use crate::metrics::common::*;
use crate::types::*;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::mem::{size_of, MaybeUninit};
use std::sync::{LazyLock, Mutex, OnceLock};

#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

static PREVIOUS_CPU_TIMES: LazyLock<Mutex<Option<CpuTimes>>> = LazyLock::new(|| Mutex::new(None));

pub fn read_cpu_percent() -> (f64, bool) {
    let times = match sysctl_value::<[libc::c_long; libc::CPUSTATES as usize]>("kern.cp_time") {
        Some(times) => times,
        None => return (0.0, false),
    };
    let idle = times[libc::CP_IDLE as usize].max(0) as u64;
    let total = times.iter().map(|time| (*time).max(0) as u64).sum();
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
    let (disk_total_mb, disk_used_mb) = freebsd_disk_usage();
    let cores = sysctl_value::<libc::c_int>("hw.ncpu").unwrap_or(1).max(1) as usize;
    static IDENTITY: OnceLock<(String, String)> = OnceLock::new();
    let identity = IDENTITY.get_or_init(|| {
        (
            sysctl_string("kern.osrelease").unwrap_or_else(|| "FreeBSD".to_string()),
            sysctl_string("hw.model").unwrap_or_else(|| native_arch().to_string()),
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
        load_avg: freebsd_load_avg(),
        boot_time_ms: freebsd_boot_time_ms(),
        os_name: "FreeBSD".to_string(),
        arch: native_arch().to_string(),
        kernel: identity.0.clone(),
        cpu_info: identity.1.clone(),
        cpu_cores: cores,
        gpu_info: detect_gpu_info(),
        processes: freebsd_process_count(),
        tcp_conn: freebsd_conn_count("tcp"),
        udp_conn: freebsd_conn_count("udp"),
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    let total_bytes = match sysctl_value::<libc::c_ulong>("hw.physmem") {
        Some(total) => total as u64,
        None => return (MemoryStats::default(), false),
    };
    let page_size = sysctl_value::<libc::c_int>("hw.pagesize")
        .unwrap_or(4096)
        .max(1) as u64;
    let free_pages = sysctl_value::<libc::c_uint>("vm.stats.vm.v_free_count").unwrap_or(0) as u64;
    let total_mb = total_bytes / BYTES_PER_MIB;
    let free_mb = free_pages.saturating_mul(page_size) / BYTES_PER_MIB;
    (
        MemoryStats {
            mem_total_mb: total_mb,
            mem_used_mb: total_mb.saturating_sub(free_mb),
            swap_total_mb: 0,
            swap_used_mb: 0,
        },
        total_mb > 0,
    )
}

pub fn read_net_bytes(ifaces: &str) -> Result<NetBytes, String> {
    let wanted = crate::storage::config::split_interface_set(ifaces);
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let mut total = NetBytes::default();
    let mut matched = 0usize;
    let mut seen = HashSet::new();
    let mut current = ifap;
    unsafe {
        while !current.is_null() {
            let item = &*current;
            if !item.ifa_name.is_null()
                && !item.ifa_data.is_null()
                && !item.ifa_addr.is_null()
                && (*item.ifa_addr).sa_family as libc::c_int == libc::AF_LINK
            {
                let name = CStr::from_ptr(item.ifa_name).to_string_lossy().into_owned();
                if seen.insert(name.clone()) && should_include_net_interface(&name, &wanted) {
                    let counters = &*(item.ifa_data as *const libc::if_data);
                    total.rx = total.rx.saturating_add(counters.ifi_ibytes as u64);
                    total.tx = total.tx.saturating_add(counters.ifi_obytes as u64);
                    matched += 1;
                }
            }
            current = item.ifa_next;
        }
        libc::freeifaddrs(ifap);
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

fn freebsd_disk_usage() -> (u64, u64) {
    let mut mount_table: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut mount_table, libc::MNT_NOWAIT) };
    if count <= 0 || mount_table.is_null() {
        return (0, 0);
    }
    let mut total = 0u64;
    let mut used = 0u64;
    for index in 0..count as usize {
        let mount = unsafe { &*mount_table.add(index) };
        let source = unsafe { CStr::from_ptr(mount.f_mntfromname.as_ptr()) }.to_string_lossy();
        if !source.starts_with("/dev/") {
            continue;
        }
        if let Some((size, used_size)) = disk_usage_mb_from_blocks(
            mount.f_blocks as u64,
            mount.f_bfree as u64,
            mount.f_bsize as i64,
        ) {
            total = total.saturating_add(size);
            used = used.saturating_add(used_size);
        }
    }
    (total, used)
}

fn freebsd_load_avg() -> String {
    let mut load = [0.0f64; 3];
    if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } == 3 {
        format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2])
    } else {
        "0 0 0".to_string()
    }
}

fn freebsd_boot_time_ms() -> i64 {
    sysctl_value::<libc::timeval>("kern.boottime")
        .map(|time| time.tv_sec.saturating_mul(1000) + (time.tv_usec / 1000) as i64)
        .unwrap_or(0)
}

fn freebsd_process_count() -> usize {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
    let mut size = 0usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return 0;
    }
    size / size_of::<libc::kinfo_proc>()
}

fn freebsd_conn_count(kind: &str) -> usize {
    if kind == "tcp" {
        return sysctl_value::<libc::c_int>("net.inet.tcp.pcbcount")
            .or_else(|| pcb_list_count("net.inet.tcp.pcblist").map(|count| count as libc::c_int))
            .unwrap_or(0)
            .max(0) as usize;
    }
    pcb_list_count("net.inet.udp.pcblist").unwrap_or(0)
}

fn pcb_list_count(name: &str) -> Option<usize> {
    const MAX_PCB_LIST_BYTES: usize = 64 * 1024 * 1024;
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
        || size < size_of::<libc::xinpgen>()
        || size > MAX_PCB_LIST_BYTES
    {
        return None;
    }

    let mut records = vec![0u8; size];
    for _ in 0..3 {
        let mut actual = records.len();
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                records.as_mut_ptr().cast(),
                &mut actual,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            records.truncate(actual.min(records.len()));
            return count_pcb_records(&records);
        }
        if actual <= records.len() || actual > MAX_PCB_LIST_BYTES {
            return None;
        }
        records.resize(actual, 0);
    }
    None
}

fn count_pcb_records(records: &[u8]) -> Option<usize> {
    let header_size = size_of::<libc::xinpgen>();
    if records.len() < header_size {
        return None;
    }
    let header = unsafe { std::ptr::read_unaligned(records.as_ptr().cast::<libc::xinpgen>()) };
    let mut offset = header.xig_len as usize;
    if offset < header_size || offset > records.len() {
        return None;
    }

    let mut count = 0usize;
    while offset + size_of::<libc::ksize_t>() <= records.len() {
        let record_size = unsafe {
            std::ptr::read_unaligned(records.as_ptr().add(offset).cast::<libc::ksize_t>()) as usize
        };
        if record_size <= header_size {
            break;
        }
        let end = offset.checked_add(record_size)?;
        if end > records.len() {
            return None;
        }
        count += 1;
        offset = end;
    }
    Some(count)
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
