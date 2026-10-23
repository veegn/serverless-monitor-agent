use serverless_monitor_agent::clock::{
    parse_http_date, CalibratedClock, ClockCalibration,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn test_date_calibration_uses_rtt_midpoint_and_monotonic_age() {
    let anchor_mono = Instant::now();
    let anchor_sys = SystemTime::now();
    let anchor_sys_ms = anchor_sys
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let date_time = anchor_sys_ms + 500;

    let cal = ClockCalibration::new_date(
        date_time,
        Duration::from_millis(80),
        anchor_sys,
        anchor_mono,
    );

    let observed_mono = anchor_mono + Duration::from_millis(20);
    let observed_sys = anchor_sys + Duration::from_millis(20);
    let snapshot = cal.snapshot(observed_sys, observed_mono);

    assert_eq!(snapshot.accurate_ts, Some(date_time + 60));
    assert_eq!(snapshot.offset_ms, Some(540));
    assert_eq!(snapshot.source.as_deref(), Some("date"));
    assert_eq!(snapshot.round_trip_ms, Some(80));
    assert_eq!(snapshot.sample_age_ms, Some(20));
}

#[test]
fn test_response_date_time_parses_http_date_header() {
    let got = parse_http_date("Thu, 13 Aug 2026 00:23:22 GMT");
    assert!(got.is_some());
    // 2026-08-13 00:23:22 UTC = 1786580602000
    assert_eq!(got.unwrap(), 1786580602000);
}

#[test]
fn test_response_date_time_rejects_missing_or_invalid_date_header() {
    assert!(parse_http_date("").is_none());
    assert!(parse_http_date("2026-08-13 00:23:22").is_none());
}

#[test]
fn test_calibrated_clock_skips_date_update_within_threshold() {
    let clock = CalibratedClock::default();
    let anchor_mono = Instant::now();
    let anchor_sys = SystemTime::now();
    let anchor_sys_ms = anchor_sys
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let (_, updated) = clock.update_date(anchor_sys_ms, Duration::ZERO, anchor_sys, anchor_mono);
    assert!(updated);

    // 15 seconds is within the 20-second threshold -> skip
    let (_, updated) = clock.update_date(
        anchor_sys_ms + 15_000,
        Duration::ZERO,
        anchor_sys,
        anchor_mono,
    );
    assert!(!updated);

    let snap = clock.snapshot(anchor_sys, anchor_mono);
    assert_eq!(snap.accurate_ts, Some(anchor_sys_ms));
}

#[test]
fn test_calibrated_clock_updates_date_beyond_threshold() {
    let clock = CalibratedClock::default();
    let anchor_mono = Instant::now();
    let anchor_sys = SystemTime::now();
    let anchor_sys_ms = anchor_sys
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let (_, updated) = clock.update_date(anchor_sys_ms, Duration::ZERO, anchor_sys, anchor_mono);
    assert!(updated);

    // 25 seconds is beyond 20-second threshold -> update
    let (_, updated) = clock.update_date(
        anchor_sys_ms + 25_000,
        Duration::ZERO,
        anchor_sys,
        anchor_mono,
    );
    assert!(updated);

    let snap = clock.snapshot(anchor_sys, anchor_mono);
    assert_eq!(snap.accurate_ts, Some(anchor_sys_ms + 25_000));
}
