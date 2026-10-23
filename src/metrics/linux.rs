use crate::metrics::common::*;
use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::sync::{Mutex, OnceLock};

static PROC_CPU: Mutex<Option<CpuTimes>> = Mutex::new(None);
static LINUX_BOOT_TIME: OnceLock<i64> = OnceLock::new();
static LINUX_OS_NAME: OnceLock<String> = OnceLock::new();
static LINUX_CPU_INFO: OnceLock<String> = OnceLock::new();

pub fn read_cpu_percent() -> (f64, bool) {
    let current = match read_proc_cpu_times() {
        Some(t) => t,
        None => return (0.0, false),
    };

    let mut lock = PROC_CPU.lock().unwrap();
    if let Some(prev) = *lock {
        *lock = Some(current);
        if let Some(pct) = cpu_usage_percent(prev, current) {
            return (pct, true);
        }
        (0.0, false)
    } else {
        *lock = Some(current);
        (0.0, false)
    }
}

pub fn read_proc_cpu_times() -> Option<CpuTimes> {
    let file = fs::File::open("/proc/stat").ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::with_capacity(128);
    reader.read_line(&mut line).ok()?;

    if line.starts_with("cpu ") {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            return None;
        }
        let mut total: u64 = 0;
        for (i, f) in fields.iter().skip(1).enumerate() {
            if i >= 8 {
                break;
            }
            if let Ok(n) = f.parse::<u64>() {
                total += n;
            }
        }
        let idle: u64 = fields[4].parse().ok()?;
        let iowait: u64 = if fields.len() > 5 {
            fields[5].parse().unwrap_or(0)
        } else {
            0
        };
        return Some(CpuTimes {
            total,
            idle: idle + iowait,
        });
    }
    None
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxMemStats {
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
}

pub fn parse_proc_meminfo() -> Option<LinuxMemStats> {
    let file = fs::File::open("/proc/meminfo").ok()?;
    let reader = BufReader::new(file);

    let mut total_kb = 0u64;
    let mut free_kb = 0u64;
    let mut available_kb = 0u64;
    let mut buffers_kb = 0u64;
    let mut cached_kb = 0u64;
    let mut swap_total_kb = 0u64;
    let mut swap_free_kb = 0u64;

    for line in reader.lines().map_while(Result::ok) {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            match key {
                "MemTotal" => total_kb = parse_first_uint(v),
                "MemFree" => free_kb = parse_first_uint(v),
                "MemAvailable" => available_kb = parse_first_uint(v),
                "Buffers" => buffers_kb = parse_first_uint(v),
                "Cached" => cached_kb = parse_first_uint(v),
                "SwapTotal" => swap_total_kb = parse_first_uint(v),
                "SwapFree" => swap_free_kb = parse_first_uint(v),
                _ => {}
            }
        }
    }

    if total_kb == 0 {
        return None;
    }

    let mem_used_mb = memory_used_mb_from_kb(total_kb, available_kb, free_kb, buffers_kb, cached_kb);
    let swap_used_mb = swap_used_mb_from_kb(swap_total_kb, swap_free_kb);

    Some(LinuxMemStats {
        mem_total_mb: total_kb / 1024,
        mem_used_mb,
        swap_total_mb: swap_total_kb / 1024,
        swap_used_mb,
    })
}

pub fn collect_basic_stats() -> BasicStats {
    let mem = parse_proc_meminfo().unwrap_or_default();
    let (disk_total, disk_used, disk_devices) = disk_usage_linux();

    let tcp_conn = get_linux_conn_count("tcp");
    let udp_conn = get_linux_conn_count("udp");

    BasicStats {
        mem_total_mb: mem.mem_total_mb,
        mem_used_mb: mem.mem_used_mb,
        swap_total_mb: mem.swap_total_mb,
        swap_used_mb: mem.swap_used_mb,
        disk_total_mb: disk_total,
        disk_used_mb: disk_used,
        disk_devices,
        load_avg: linux_load_avg(),
        boot_time_ms: linux_boot_time_ms(),
        os_name: linux_os_name(),
        arch: native_arch().to_string(),
        kernel: crate::util::command::command_output("uname", &["-r"]),
        cpu_info: linux_cpu_info(),
        cpu_cores: num_cpus(),
        gpu_info: detect_gpu_info(),
        processes: linux_process_count(),
        tcp_conn,
        udp_conn,
    }
}

pub fn read_memory_stats() -> (MemoryStats, bool) {
    if let Some(mem) = parse_proc_meminfo() {
        (
            MemoryStats {
                mem_total_mb: mem.mem_total_mb,
                mem_used_mb: mem.mem_used_mb,
                swap_total_mb: mem.swap_total_mb,
                swap_used_mb: mem.swap_used_mb,
            },
            true,
        )
    } else {
        (MemoryStats::default(), false)
    }
}

pub fn read_mem_info() -> HashMap<String, u64> {
    let mut map = HashMap::new();
    if let Ok(file) = fs::File::open("/proc/meminfo") {
        let reader = BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            if let Some((k, v)) = line.split_once(':') {
                map.insert(k.trim().to_string(), parse_first_uint(v));
            }
        }
    }
    map
}

pub fn linux_load_avg() -> String {
    if let Ok(content) = fs::read_to_string("/proc/loadavg") {
        let fields: Vec<&str> = content.split_whitespace().collect();
        if fields.len() >= 3 {
            return format!("{} {} {}", fields[0], fields[1], fields[2]);
        }
    }
    "0 0 0".to_string()
}

pub fn linux_boot_time_ms() -> i64 {
    *LINUX_BOOT_TIME.get_or_init(|| {
        if let Ok(file) = fs::File::open("/proc/stat") {
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                if line.starts_with("btime ") {
                    if let Some(sec_str) = line.split_whitespace().nth(1) {
                        if let Ok(btime) = sec_str.parse::<i64>() {
                            return btime * 1000;
                        }
                    }
                }
            }
        }
        if let Ok(content) = fs::read_to_string("/proc/uptime") {
            if let Some(up_str) = content.split_whitespace().next() {
                if let Ok(uptime_secs) = up_str.parse::<f64>() {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    return now_ms - (uptime_secs * 1000.0) as i64;
                }
            }
        }
        0
    })
}

pub fn linux_os_name() -> String {
    LINUX_OS_NAME
        .get_or_init(|| {
            if let Ok(content) = fs::read_to_string("/etc/os-release") {
                let mut pretty = String::new();
                let mut id = String::new();
                for line in content.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let v = v.trim().trim_matches('"');
                        if k == "PRETTY_NAME" {
                            pretty = v.to_string();
                        } else if k == "ID" {
                            id = v.to_string();
                        }
                    }
                }
                if !pretty.is_empty() {
                    return pretty;
                }
                if !id.is_empty() {
                    return id;
                }
            }
            "Linux".to_string()
        })
        .clone()
}

pub fn linux_cpu_info() -> String {
    LINUX_CPU_INFO
        .get_or_init(|| {
            if let Ok(file) = fs::File::open("/proc/cpuinfo") {
                let reader = BufReader::new(file);
                for line in reader.lines().map_while(Result::ok) {
                    if line.starts_with("model name")
                        || line.starts_with("Hardware")
                        || line.starts_with("Processor")
                        || line.starts_with("cpu model")
                    {
                        if let Some((_, v)) = line.split_once(':') {
                            let s = v.trim();
                            if !s.is_empty() {
                                return s.to_string();
                            }
                        }
                    }
                }
            }
            native_arch().to_string()
        })
        .clone()
}

pub fn linux_process_count() -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut count = 0;
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let bytes = entry.file_name();
                let s = bytes.as_bytes();
                if !s.is_empty() && s.iter().all(|b| b.is_ascii_digit()) {
                    count += 1;
                }
            }
        }
        count
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub fn get_linux_conn_count(proto: &str) -> usize {
    if let Some(c) = sockstat_conn_count(proto) {
        let c6 = sockstat6_conn_count(proto).unwrap_or(0);
        return c + c6;
    }
    let p6 = format!("{}6", proto);
    linux_conn_count(proto) + linux_conn_count(&p6)
}

fn sockstat_conn_count(proto: &str) -> Option<usize> {
    let file = fs::File::open("/proc/net/sockstat").ok()?;
    let reader = BufReader::new(file);
    let prefix = if proto == "tcp" { "TCP:" } else { "UDP:" };
    for line in reader.lines().map_while(Result::ok) {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some((_, inuse_rest)) = rest.split_once("inuse ") {
                let num_str = inuse_rest.split_whitespace().next().unwrap_or("");
                if let Ok(count) = num_str.parse::<usize>() {
                    return Some(count);
                }
            }
        }
    }
    None
}

fn sockstat6_conn_count(proto: &str) -> Option<usize> {
    let file = fs::File::open("/proc/net/sockstat6").ok()?;
    let reader = BufReader::new(file);
    let prefix = if proto == "tcp" { "TCP6:" } else { "UDP6:" };
    for line in reader.lines().map_while(Result::ok) {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some((_, inuse_rest)) = rest.split_once("inuse ") {
                let num_str = inuse_rest.split_whitespace().next().unwrap_or("");
                if let Ok(count) = num_str.parse::<usize>() {
                    return Some(count);
                }
            }
        }
    }
    None
}

pub fn linux_conn_count(name: &str) -> usize {
    let mut count = 0;
    let path = format!("/proc/net/{}", name);
    if let Ok(file) = fs::File::open(&path) {
        let reader = BufReader::new(file);
        for line in reader.lines().skip(1).map_while(Result::ok) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 4 {
                if name.starts_with("tcp") {
                    if fields[3] == "01" {
                        count += 1;
                    }
                } else {
                    count += 1;
                }
            }
        }
    }
    count
}

pub fn read_net_bytes(ifaces: &str) -> Result<NetBytes, String> {
    let wanted: HashSet<String> = crate::storage::config::split_interface_set(ifaces);
    let file = fs::File::open("/proc/net/dev").map_err(|e| e.to_string())?;
    let reader = BufReader::new(file);

    let mut total = NetBytes::default();
    let mut matched = 0;

    for line in reader.lines().map_while(Result::ok) {
        if !line.contains(':') {
            continue;
        }
        let (name, rest) = match line.split_once(':') {
            Some(pair) => (pair.0.trim(), pair.1.trim()),
            None => continue,
        };
        if !should_include_net_interface(name, &wanted) {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 16 {
            return Err(format!("invalid network counters for {}", name));
        }
        let rx: u64 = fields[0]
            .parse()
            .map_err(|_| format!("invalid network counters for {}", name))?;
        let tx: u64 = fields[8]
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

struct DiskUsageEntry {
    total: u64,
    used: u64,
    device: DiskDeviceRef,
    has_device: bool,
}

pub fn disk_usage_linux() -> (u64, u64, Vec<DiskDeviceRef>) {
    let mut devices: HashMap<String, DiskUsageEntry> = HashMap::new();
    let file = match fs::File::open("/proc/mounts") {
        Ok(f) => f,
        Err(_) => return (0, 0, Vec::new()),
    };
    let reader = BufReader::new(file);

    for line in reader.lines().map_while(Result::ok) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let dev = fields[0];
        let mount_point = unescape_linux_mount_point(fields[1]);
        let fs_type = fields[2];
        let opts = if fields.len() >= 4 { fields[3] } else { "" };

        if !include_linux_mount(dev, &mount_point, fs_type, opts) {
            continue;
        }

        #[cfg(target_os = "linux")]
        {
            use std::ffi::CString;
            use std::mem::MaybeUninit;
            if let Ok(c_mount) = CString::new(mount_point.as_str()) {
                let mut st: MaybeUninit<libc::statfs> = MaybeUninit::uninit();
                let res = unsafe { libc::statfs(c_mount.as_ptr(), st.as_mut_ptr()) };
                if res == 0 {
                    let st = unsafe { st.assume_init() };
                    let bsize = st.f_bsize as i64;
                    let blocks = st.f_blocks;
                    let bfree = st.f_bfree;
                    if let Some((size, used)) = disk_usage_mb_from_blocks(blocks, bfree, bsize) {
                        let mut device_id = dev.to_string();
                        if fs_type.eq_ignore_ascii_case("zfs") {
                            if let Some((pool, _)) = device_id.split_once('/') {
                                device_id = pool.to_string();
                            }
                        }
                        let (device, has_device) = linux_disk_device_ref(dev, &mount_point, &device_id);
                        let entry = DiskUsageEntry {
                            total: size,
                            used,
                            device,
                            has_device,
                        };

                        let fsid_key = {
                            let (v0, v1): (libc::c_int, libc::c_int) = unsafe {
                                if std::mem::size_of::<libc::fsid_t>() == std::mem::size_of::<(libc::c_int, libc::c_int)>() {
                                    std::mem::transmute_copy(&st.f_fsid)
                                } else {
                                    (0, 0)
                                }
                            };
                            if v0 == 0 && v1 == 0 {
                                String::new()
                            } else {
                                format!("fsid:{}:{}", v0, v1)
                            }
                        };

                        let mut dedup_key = if fsid_key.is_empty() {
                            device_id.clone()
                        } else {
                            fsid_key
                        };

                        if let Some(existing) = devices.get(&dedup_key) {
                            if distinct_block_devices(&device_id, &existing.device.key) {
                                dedup_key = device_id;
                            }
                        }

                        let should_insert = match devices.get(&dedup_key) {
                            None => true,
                            Some(existing) => disk_entry_preferred(&entry, existing),
                        };
                        if should_insert {
                            devices.insert(dedup_key, entry);
                        }
                    }
                }
            }
        }
    }

    let mut total: u64 = 0;
    let mut used: u64 = 0;
    let mut disk_devices = Vec::new();
    for entry in devices.values() {
        total += entry.total;
        used += entry.used;
        if entry.has_device {
            disk_devices.push(entry.device.clone());
        }
    }
    disk_devices.sort_by(|a, b| a.key.cmp(&b.key));
    (total, used, disk_devices)
}

fn distinct_block_devices(a: &str, b: &str) -> bool {
    a.starts_with("/dev/") && b.starts_with("/dev/") && a != b
}

fn disk_entry_preferred(entry: &DiskUsageEntry, existing: &DiskUsageEntry) -> bool {
    if entry.total != existing.total {
        return entry.total > existing.total;
    }
    entry.device.major != 0 && existing.device.major == 0
}

#[cfg(target_os = "linux")]
fn linux_disk_device_ref(dev: &str, mount_point: &str, key: &str) -> (DiskDeviceRef, bool) {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let dev_path = unescape_linux_mount_point(dev);
    if dev_path.starts_with("/dev/") {
        if let Ok(c_dev) = CString::new(dev_path) {
            let mut st: MaybeUninit<libc::stat> = MaybeUninit::uninit();
            if unsafe { libc::stat(c_dev.as_ptr(), st.as_mut_ptr()) } == 0 {
                let st = unsafe { st.assume_init() };
                if st.st_rdev != 0 {
                    let (major, minor) = linux_device_major_minor(st.st_rdev as u64);
                    if major != 0 || minor != 0 {
                        return (
                            DiskDeviceRef {
                                key: key.to_string(),
                                major,
                                minor,
                            },
                            true,
                        );
                    }
                }
            }
        }
    }

    if let Ok(c_mount) = CString::new(mount_point) {
        let mut st: MaybeUninit<libc::stat> = MaybeUninit::uninit();
        if unsafe { libc::stat(c_mount.as_ptr(), st.as_mut_ptr()) } == 0 {
            let st = unsafe { st.assume_init() };
            if st.st_dev != 0 {
                let (major, minor) = linux_device_major_minor(st.st_dev as u64);
                if major != 0 || minor != 0 {
                    return (
                        DiskDeviceRef {
                            key: key.to_string(),
                            major,
                            minor,
                        },
                        true,
                    );
                }
            }
        }
    }
    (DiskDeviceRef::default(), false)
}

#[cfg(not(target_os = "linux"))]
fn linux_disk_device_ref(_dev: &str, _mount_point: &str, _key: &str) -> (DiskDeviceRef, bool) {
    (DiskDeviceRef::default(), false)
}

#[allow(dead_code)]
fn linux_device_major_minor(dev: u64) -> (u64, u64) {
    let mut major = (dev & 0x00000000000fff00) >> 8;
    major |= (dev & 0xfffff00000000000) >> 32;
    let mut minor = dev & 0x00000000000000ff;
    minor |= (dev & 0x00000ffffff00000) >> 12;
    (major, minor)
}

pub fn read_disk_io_counters(devices: &[DiskDeviceRef]) -> DiskIOCounters {
    let targets = linux_disk_io_targets(devices);
    if targets.is_empty() {
        return DiskIOCounters::default();
    }
    let mut total = DiskIOCounters::default();
    let mut matched = HashSet::new();

    if let Ok(file) = fs::File::open("/proc/diskstats") {
        let reader = BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 14 {
                continue;
            }
            let major: u64 = match fields[0].parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let minor: u64 = match fields[1].parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let id = format!("{}:{}", major, minor);
            if !targets.contains(&id) {
                continue;
            }
            let read_ops: u64 = fields[3].parse().unwrap_or(0);
            let read_sectors: u64 = fields[5].parse().unwrap_or(0);
            let read_time: u64 = fields[6].parse().unwrap_or(0);
            let write_ops: u64 = fields[7].parse().unwrap_or(0);
            let write_sectors: u64 = fields[9].parse().unwrap_or(0);
            let write_time: u64 = fields[10].parse().unwrap_or(0);
            let io_ticks: u64 = fields[12].parse().unwrap_or(0);

            total.read_ops += read_ops;
            total.write_ops += write_ops;
            total.read_bytes += read_sectors * 512;
            total.write_bytes += write_sectors * 512;
            total.read_time_ms += read_time;
            total.write_time_ms += write_time;
            total.io_ticks_ms += io_ticks;
            matched.insert(id);
        }
    }

    let mut keys: Vec<String> = matched.into_iter().collect();
    keys.sort();
    total.device_count = keys.len();
    total.fingerprint = keys.join(",");
    total
}

fn linux_disk_io_targets(devices: &[DiskDeviceRef]) -> HashSet<String> {
    let mut targets = HashSet::new();
    for device in devices {
        if device.major == 0 && device.minor == 0 {
            continue;
        }
        targets.insert(format!("{}:{}", device.major, device.minor));
    }
    targets
}

fn include_linux_mount(dev: &str, mount_point: &str, fs_type: &str, opts: &str) -> bool {
    if mount_point == "/" {
        return true;
    }
    let mp_lower = mount_point.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "/tmp",
        "/var/tmp",
        "/dev",
        "/run",
        "/var/lib/containers",
        "/var/lib/docker",
        "/proc",
        "/sys",
        "/sys/fs/cgroup",
        "/etc/resolv.conf",
        "/etc/host",
        "/nix/store",
    ];
    for &p in PREFIXES {
        if mp_lower == p || mp_lower.starts_with(&format!("{}/", p)) {
            return false;
        }
    }
    let fs_lower = fs_type.to_ascii_lowercase();
    if fs_lower == "autofs" && !dev.starts_with("/dev/") {
        return false;
    }
    if fs_lower == "fuseblk" {
        return true;
    }
    const EXCLUDED_FS: &[&str] = &[
        "tmpfs", "devtmpfs", "udev", "nfs", "cifs", "smb", "vboxsf", "9p", "fuse", "overlay", "proc", "devpts",
        "sysfs", "cgroup", "mqueue", "hugetlbfs", "debugfs", "binfmt_misc", "securityfs",
    ];
    for &exc in EXCLUDED_FS {
        if fs_lower == exc || fs_lower.starts_with(exc) {
            return false;
        }
    }
    let opts_lower = opts.to_ascii_lowercase();
    if opts_lower.contains("remote") || opts_lower.contains("network") {
        return false;
    }
    if dev.starts_with("/dev/loop") {
        return false;
    }
    true
}

fn unescape_linux_mount_point(path: &str) -> String {
    path.replace(r"\040", " ")
        .replace(r"\011", "\t")
        .replace(r"\012", "\n")
        .replace(r"\134", "\\")
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
