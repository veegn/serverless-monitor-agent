use crate::types::ReportTime;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CALIBRATION_AGE: Duration = Duration::from_secs(24 * 3600);
const DATE_CALIBRATION_THRESHOLD_MS: u64 = 20_000;

#[derive(Debug, Clone)]
pub struct ClockCalibration {
    pub source: String,
    pub anchor_sys: SystemTime,
    pub anchor_mono: Instant,
    pub accurate_at_anchor: i64,
    pub round_trip_ms: u64,
}

impl ClockCalibration {
    pub fn new_date(date_time_ms: i64, round_trip: Duration, anchor_sys: SystemTime, anchor_mono: Instant) -> Self {
        let round_trip_ms = round_trip.as_millis() as u64;
        let half_round_trip = round_trip_ms / 2 + round_trip_ms % 2;
        let accurate_at_anchor = add_milliseconds(date_time_ms, half_round_trip);
        Self {
            source: "date".to_string(),
            anchor_sys,
            anchor_mono,
            accurate_at_anchor,
            round_trip_ms,
        }
    }

    pub fn timestamp(&self, mono: Instant) -> i64 {
        if mono >= self.anchor_mono {
            let delta = mono.duration_since(self.anchor_mono).as_millis() as i64;
            saturating_add(self.accurate_at_anchor, delta)
        } else {
            let delta = self.anchor_mono.duration_since(mono).as_millis() as i64;
            saturating_subtract(self.accurate_at_anchor, delta)
        }
    }

    pub fn snapshot(&self, sys: SystemTime, mono: Instant) -> ReportTime {
        let age_ms = if mono >= self.anchor_mono {
            mono.duration_since(self.anchor_mono).as_millis() as u64
        } else {
            0
        };
        let accurate_ts = self.timestamp(mono);
        let local_ts = sys.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let offset_ms = saturating_subtract(accurate_ts, local_ts);
        ReportTime {
            local_ts,
            accurate_ts: Some(accurate_ts),
            offset_ms: Some(offset_ms),
            source: Some(self.source.clone()),
            round_trip_ms: Some(self.round_trip_ms),
            sample_age_ms: Some(age_ms),
        }
    }
}

#[derive(Debug, Default)]
pub struct CalibratedClock {
    calibration: Mutex<Option<ClockCalibration>>,
}

impl CalibratedClock {
    pub fn new() -> Self {
        Self {
            calibration: Mutex::new(None),
        }
    }

    pub fn snapshot(&self, sys: SystemTime, mono: Instant) -> ReportTime {
        let lock = self.calibration.lock().unwrap();
        if let Some(ref cal) = *lock {
            if Self::is_usable(cal, mono, false) {
                return cal.snapshot(sys, mono);
            }
        }
        let local_ts = sys.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        ReportTime {
            local_ts,
            accurate_ts: None,
            offset_ms: None,
            source: None,
            round_trip_ms: None,
            sample_age_ms: None,
        }
    }

    pub fn timestamp(&self, mono: Instant) -> (i64, bool) {
        let lock = self.calibration.lock().unwrap();
        if let Some(ref cal) = *lock {
            if Self::is_usable(cal, mono, true) {
                return (cal.timestamp(mono), true);
            }
        }
        let now_mono = Instant::now();
        let now_sys = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let ts = if mono <= now_mono {
            let elapsed_ms = now_mono.duration_since(mono).as_millis() as i64;
            saturating_subtract(now_sys, elapsed_ms)
        } else {
            let future_ms = mono.duration_since(now_mono).as_millis() as i64;
            saturating_add(now_sys, future_ms)
        };
        (ts, false)
    }

    pub fn correct_local_timestamp(&self, local_ts: i64, observed_mono: Instant, observed_sys: SystemTime) -> i64 {
        let (accurate_at, ok) = self.timestamp(observed_mono);
        if !ok {
            return local_ts;
        }
        let observed_sys_ms = observed_sys.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let offset = saturating_subtract(accurate_at, observed_sys_ms);
        saturating_add(local_ts, offset)
    }

    pub fn update_date(
        &self,
        date_time_ms: i64,
        round_trip: Duration,
        anchor_sys: SystemTime,
        anchor_mono: Instant,
    ) -> (ReportTime, bool) {
        let new_cal = ClockCalibration::new_date(date_time_ms, round_trip, anchor_sys, anchor_mono);
        let now_sys = SystemTime::now();
        let now_mono = Instant::now();

        let mut lock = self.calibration.lock().unwrap();
        if let Some(ref current) = *lock {
            if Self::is_usable(current, anchor_mono, false) {
                let cur_ts = current.timestamp(anchor_mono);
                if absolute_difference(cur_ts, new_cal.accurate_at_anchor) <= DATE_CALIBRATION_THRESHOLD_MS {
                    return (current.snapshot(now_sys, now_mono), false);
                }
            }
        }
        *lock = Some(new_cal.clone());
        (new_cal.snapshot(now_sys, now_mono), true)
    }

    fn is_usable(cal: &ClockCalibration, at: Instant, allow_retroactive: bool) -> bool {
        if at >= cal.anchor_mono {
            at.duration_since(cal.anchor_mono) <= MAX_CALIBRATION_AGE
        } else {
            allow_retroactive && cal.anchor_mono.duration_since(at) <= MAX_CALIBRATION_AGE
        }
    }
}

pub fn add_milliseconds(timestamp: i64, ms: u64) -> i64 {
    if ms > i64::MAX as u64 || timestamp > i64::MAX - ms as i64 {
        i64::MAX
    } else {
        timestamp + ms as i64
    }
}

pub fn saturating_subtract(left: i64, right: i64) -> i64 {
    left.saturating_sub(right)
}

pub fn saturating_add(left: i64, right: i64) -> i64 {
    left.saturating_add(right)
}

pub fn absolute_difference(left: i64, right: i64) -> u64 {
    left.abs_diff(right)
}

pub fn parse_http_date(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Try RFC 7231 / 2822 / 850 formats
    // e.g. "Thu, 13 Aug 2026 00:23:22 GMT"
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a, %d-%b-%y %H:%M:%S GMT") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%c") {
        return Some(dt.and_utc().timestamp_millis());
    }
    None
}
