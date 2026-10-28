use chrono::{DateTime, Local, TimeZone};
use serverless_monitor_agent::storage::traffic::{
    calc_monthly_traffic, period_start_ts, read_monthly_traffic_totals, read_traffic_state,
    write_traffic_state, TrafficState,
};
use serverless_monitor_agent::types::NetBytes;

#[test]
fn test_read_monthly_traffic_totals() {
    let now = Local.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap();
    let current_period = period_start_ts(now, 1);
    let prev_month = Local.with_ymd_and_hms(2026, 9, 22, 12, 0, 0).unwrap();
    let previous_period = period_start_ts(prev_month, 1);
    let legacy_midnight = Local.with_ymd_and_hms(2026, 10, 1, 0, 30, 0).unwrap();

    struct TestCase {
        name: &'static str,
        reset_day: u32,
        period_start: i64,
        last_check: i64,
        iface: &'static str,
        want_rx: u64,
        want_tx: u64,
    }

    let cases = vec![
        TestCase {
            name: "current period",
            reset_day: 1,
            period_start: current_period,
            last_check: now.timestamp(),
            iface: "eth0",
            want_rx: 500,
            want_tx: 600,
        },
        TestCase {
            name: "previous period",
            reset_day: 1,
            period_start: previous_period,
            last_check: prev_month.timestamp(),
            iface: "eth0",
            want_rx: 0,
            want_tx: 0,
        },
        TestCase {
            name: "different interface",
            reset_day: 1,
            period_start: current_period,
            last_check: now.timestamp(),
            iface: "eth1",
            want_rx: 0,
            want_tx: 0,
        },
        TestCase {
            name: "uninitialized state",
            reset_day: 1,
            period_start: current_period,
            last_check: 0,
            iface: "eth0",
            want_rx: 0,
            want_tx: 0,
        },
        TestCase {
            name: "no reset",
            reset_day: 0,
            period_start: previous_period,
            last_check: prev_month.timestamp(),
            iface: "eth0",
            want_rx: 500,
            want_tx: 600,
        },
        TestCase {
            name: "legacy current period",
            reset_day: 1,
            period_start: 0,
            last_check: now.timestamp(),
            iface: "eth0",
            want_rx: 500,
            want_tx: 600,
        },
        TestCase {
            name: "legacy previous period",
            reset_day: 1,
            period_start: 0,
            last_check: prev_month.timestamp(),
            iface: "eth0",
            want_rx: 0,
            want_tx: 0,
        },
        TestCase {
            name: "legacy local midnight",
            reset_day: 1,
            period_start: 0,
            last_check: legacy_midnight.timestamp(),
            iface: "eth0",
            want_rx: 500,
            want_tx: 600,
        },
    ];

    let dir = std::env::temp_dir().join(format!("test-traffic-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    for tc in cases {
        let path = dir.join(format!("{}.dat", tc.name.replace(' ', "_")));
        let st = TrafficState {
            rx_prev: 100,
            tx_prev: 200,
            rx_period: 500,
            tx_period: 600,
            last_check: tc.last_check,
            period_start: tc.period_start,
            interface: tc.iface.to_string(),
        };
        write_traffic_state(&path, &st).expect("writeTrafficState failed");
        let (rx, tx) = read_monthly_traffic_totals(&path, tc.reset_day, "eth0", now);
        assert_eq!(rx, tc.want_rx, "{}: rx mismatch", tc.name);
        assert_eq!(tx, tc.want_tx, "{}: tx mismatch", tc.name);

        let got = read_traffic_state(&path);
        assert_eq!(got, st, "{}: read changed persisted state", tc.name);
    }

    let missing = dir.join("missing").join("traffic.dat");
    let (rx, tx) = read_monthly_traffic_totals(&missing, 1, "eth0", now);
    assert_eq!(rx, 0);
    assert_eq!(tx, 0);
    assert!(!missing.parent().unwrap().exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_period_start_ts_no_reset() {
    let now = Local.with_ymd_and_hms(2026, 8, 4, 0, 0, 0).unwrap();
    assert_eq!(period_start_ts(now, 0), 0);
}

#[test]
fn test_period_start_ts_clamps_reset_day() {
    let now = Local.with_ymd_and_hms(2026, 2, 15, 12, 0, 0).unwrap();
    let got_ts = period_start_ts(now, 31);
    let got = DateTime::from_timestamp(got_ts, 0)
        .unwrap()
        .with_timezone(&Local);
    let want = Local.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap();
    assert_eq!(got, want);
}

#[test]
fn test_period_start_ts_rolls_missing_reset_day_to_next_month() {
    let now = Local.with_ymd_and_hms(2026, 4, 30, 12, 0, 0).unwrap();
    let got_ts = period_start_ts(now, 31);
    let got = DateTime::from_timestamp(got_ts, 0)
        .unwrap()
        .with_timezone(&Local);
    let want = Local.with_ymd_and_hms(2026, 3, 31, 0, 0, 0).unwrap();
    assert_eq!(got, want);
}

#[test]
fn test_calc_monthly_traffic_basic() {
    let dir = std::env::temp_dir().join(format!("test-calc-traffic-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("traffic.dat");

    // First call: initial calibration
    let (rx, tx) = calc_monthly_traffic(&path, NetBytes { rx: 1000, tx: 2000 }, 1, "eth0")
        .expect("calcMonthlyTraffic failed");
    assert_eq!(rx, 0);
    assert_eq!(tx, 0);

    // Second call: increment
    let (rx, tx) = calc_monthly_traffic(&path, NetBytes { rx: 1500, tx: 2800 }, 1, "eth0")
        .expect("calcMonthlyTraffic failed");
    assert_eq!(rx, 500);
    assert_eq!(tx, 800);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_live_traffic_accumulates_between_coalesced_checkpoints() {
    use serverless_monitor_agent::storage::traffic::{read_traffic_state, TrafficTracker};
    let path = std::env::temp_dir().join(format!("sma-traffic-tracker-{}.dat", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let path = path.to_str().unwrap();
    let mut tracker = TrafficTracker::new(path);
    let now = std::time::Instant::now();
    let date = chrono::Local::now();
    tracker
        .sample(
            path,
            NetBytes { rx: 100, tx: 200 },
            0,
            "eth-test",
            date,
            now,
        )
        .unwrap();
    let on_disk = std::fs::read(path).unwrap();
    assert_eq!(
        tracker
            .sample(
                path,
                NetBytes { rx: 150, tx: 270 },
                0,
                "eth-test",
                date,
                now + std::time::Duration::from_secs(1)
            )
            .unwrap(),
        (50, 70)
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        on_disk,
        "live observations must not rewrite the checkpoint each second"
    );
    assert_eq!(
        tracker
            .sample(
                path,
                NetBytes { rx: 180, tx: 300 },
                0,
                "eth-test",
                date,
                now + std::time::Duration::from_secs(31)
            )
            .unwrap(),
        (80, 100)
    );
    let stored = read_traffic_state(path);
    assert_eq!((stored.rx_period, stored.tx_period), (80, 100));
    let _ = std::fs::remove_file(path);
}
