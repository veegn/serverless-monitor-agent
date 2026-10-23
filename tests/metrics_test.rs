use serverless_monitor_agent::metrics::common::{
    cpu_percent_string, cpu_usage_percent, disk_io_stats_from_counters,
    disk_usage_mb_from_blocks, should_include_net_interface, CpuTimes, BYTES_PER_MIB,
};
use serverless_monitor_agent::types::{DiskIOCounters, DiskIOStats};
use std::collections::HashSet;

#[test]
fn test_cpu_usage_percent_from_zero_previous() {
    let prev = CpuTimes::default();
    let current = CpuTimes {
        total: 100,
        idle: 75,
    };
    let got = cpu_usage_percent(prev, current);
    assert!(got.is_some());
    assert_eq!(got.unwrap(), 25.0);
}

#[test]
fn test_cpu_percent_string_reports_zero_as_zero() {
    assert_eq!(cpu_percent_string(0.0), "0.00");
}

#[test]
fn test_cpu_percent_string_reports_percent_units() {
    assert_eq!(cpu_percent_string(5.0), "5.00");
}

#[test]
fn test_disk_usage_mb_from_blocks() {
    let (total, used) = disk_usage_mb_from_blocks(100, 65, BYTES_PER_MIB as i64).unwrap();
    assert_eq!(total, 100);
    assert_eq!(used, 35);
}

#[test]
fn test_disk_io_stats_from_counters() {
    let prev = DiskIOCounters {
        read_bytes: 1024,
        write_bytes: 2048,
        read_ops: 10,
        write_ops: 20,
        read_time_ms: 100,
        write_time_ms: 300,
        io_ticks_ms: 1000,
        device_count: 2,
        fingerprint: "8:1,8:2".to_string(),
    };
    let current = DiskIOCounters {
        read_bytes: 3072,
        write_bytes: 6144,
        read_ops: 14,
        write_ops: 28,
        read_time_ms: 180,
        write_time_ms: 480,
        io_ticks_ms: 1600,
        device_count: 2,
        fingerprint: "8:1,8:2".to_string(),
    };

    let got = disk_io_stats_from_counters(prev, current, 2.0);
    assert_eq!(got.read_bps, 1024);
    assert_eq!(got.write_bps, 2048);
    assert_eq!(got.read_iops, 2.0);
    assert_eq!(got.write_iops, 4.0);
    assert!((got.await_ms - 21.67).abs() < 0.01);
    assert_eq!(got.util, 15.0);
}

#[test]
fn test_disk_io_stats_from_counters_rejects_changed_device_set() {
    let prev = DiskIOCounters {
        read_bytes: 1024,
        device_count: 1,
        fingerprint: "8:1".to_string(),
        ..Default::default()
    };
    let current = DiskIOCounters {
        read_bytes: 4096,
        device_count: 1,
        fingerprint: "8:2".to_string(),
        ..Default::default()
    };

    let got = disk_io_stats_from_counters(prev, current, 30.0);
    assert_eq!(got, DiskIOStats::default());
}

#[test]
fn test_should_include_net_interface_excludes_tunnel_interfaces() {
    let excluded = vec![
        "tailscale0", "tun0", "wg0", "wireguard0", "ipsec0", "gre0", "gretap0", "ipip0",
        "sit0", "ip6tnl0", "zerotier0",
    ];
    let empty_set = HashSet::new();
    for name in excluded {
        assert!(
            !should_include_net_interface(name, &empty_set),
            "interface {} should be excluded",
            name
        );
    }
}
