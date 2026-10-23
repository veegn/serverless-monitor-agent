use crate::types::{MAX_TRAFFIC_CORRECTION_GB, NetBytes};
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrafficState {
    pub rx_prev: u64,
    pub tx_prev: u64,
    pub rx_period: u64,
    pub tx_period: u64,
    pub last_check: i64,
    pub period_start: i64,
    pub interface: String,
}

pub fn read_traffic_state<P: AsRef<Path>>(path: P) -> TrafficState {
    let values = match parse_kv_file(path.as_ref()) {
        Ok(v) => v,
        Err(_) => return TrafficState::default(),
    };
    TrafficState {
        rx_prev: values.get("RX_PREV").and_then(|v| v.parse().ok()).unwrap_or(0),
        tx_prev: values.get("TX_PREV").and_then(|v| v.parse().ok()).unwrap_or(0),
        rx_period: values.get("RX_PERIOD").and_then(|v| v.parse().ok()).unwrap_or(0),
        tx_period: values.get("TX_PERIOD").and_then(|v| v.parse().ok()).unwrap_or(0),
        last_check: values.get("LAST_CHECK").and_then(|v| v.parse().ok()).unwrap_or(0),
        period_start: values.get("PERIOD_START").and_then(|v| v.parse().ok()).unwrap_or(0),
        interface: values.get("INTERFACE").cloned().unwrap_or_default(),
    }
}

pub fn read_monthly_traffic_totals<P: AsRef<Path>>(
    path: P,
    reset_day: u32,
    iface: &str,
    now: chrono::DateTime<Local>,
) -> (u64, u64) {
    let st = read_traffic_state(path);
    if st.last_check == 0 || st.interface != iface {
        return (0, 0);
    }
    let current_period = period_start_ts(now, reset_day);
    if current_period != 0 {
        let mut saved_period = st.period_start;
        if saved_period == 0 {
            if let Some(dt) = Local.timestamp_opt(st.last_check, 0).single() {
                saved_period = period_start_ts(dt, reset_day);
            }
        }
        if saved_period != current_period {
            return (0, 0);
        }
    }
    (st.rx_period, st.tx_period)
}

pub fn write_traffic_state<P: AsRef<Path>>(path: P, st: &TrafficState) -> std::io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = format!(
        "RX_PREV={}\nTX_PREV={}\nRX_PERIOD={}\nTX_PERIOD={}\nLAST_CHECK={}\nPERIOD_START={}\nINTERFACE={}\n",
        st.rx_prev, st.tx_prev, st.rx_period, st.tx_period, st.last_check, st.period_start, st.interface
    );
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path)
}

pub fn calc_monthly_traffic<P: AsRef<Path>>(
    path: P,
    current: NetBytes,
    reset_day: u32,
    iface: &str,
) -> std::io::Result<(u64, u64)> {
    let now_dt = Local::now();
    let now = now_dt.timestamp();
    let mut st = read_traffic_state(&path);
    if st.interface != iface {
        st = TrafficState::default();
    }
    let period_start = period_start_ts(now_dt, reset_day);
    if st.last_check == 0 {
        st.rx_prev = current.rx;
        st.tx_prev = current.tx;
        st.last_check = now;
        st.period_start = period_start;
        st.interface = iface.to_string();
        write_traffic_state(&path, &st)?;
        return Ok((st.rx_period, st.tx_period));
    }

    let rx_delta = current.rx.saturating_sub(st.rx_prev);
    let tx_delta = current.tx.saturating_sub(st.tx_prev);

    if period_start != 0 && st.period_start != 0 && period_start != st.period_start {
        st.rx_period = rx_delta;
        st.tx_period = tx_delta;
    } else {
        st.rx_period += rx_delta;
        st.tx_period += tx_delta;
    }

    st.rx_prev = current.rx;
    st.tx_prev = current.tx;
    st.last_check = now;
    st.period_start = period_start;
    st.interface = iface.to_string();
    write_traffic_state(&path, &st)?;
    Ok((st.rx_period, st.tx_period))
}

pub fn apply_traffic_correction_from_network<P: AsRef<Path>>(
    path: P,
    iface: &str,
    rx_gb: &str,
    tx_gb: &str,
) -> Result<(), String> {
    let current = crate::metrics::read_net_bytes(iface)
        .map_err(|e| format!("read network counters before traffic correction: {}", e))?;
    apply_traffic_correction(path, current, iface, rx_gb, tx_gb)
}

pub fn apply_traffic_correction<P: AsRef<Path>>(
    path: P,
    current: NetBytes,
    iface: &str,
    rx_gb: &str,
    tx_gb: &str,
) -> Result<(), String> {
    let rx = parse_traffic_correction_gb(rx_gb)?;
    let tx = parse_traffic_correction_gb(tx_gb)?;
    let mut st = read_traffic_state(&path);
    st.rx_prev = current.rx;
    st.tx_prev = current.tx;
    st.rx_period = rx;
    st.tx_period = tx;
    st.last_check = Local::now().timestamp();
    st.interface = iface.to_string();
    write_traffic_state(&path, &st).map_err(|e| e.to_string())
}

pub fn period_start_ts(now: chrono::DateTime<Local>, reset_day: u32) -> i64 {
    if reset_day == 0 {
        return 0;
    }
    if !(1..=31).contains(&reset_day) {
        return now.timestamp();
    }
    last_reset_date(now, reset_day).timestamp()
}

pub fn last_reset_date(now: chrono::DateTime<Local>, reset_day: u32) -> chrono::DateTime<Local> {
    let year = now.year();
    let month = now.month();
    let this_month = actual_reset_date(year, month, reset_day);
    if now >= this_month {
        return this_month;
    }
    let (prev_year, prev_month) = if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    };
    actual_reset_date(prev_year, prev_month, reset_day)
}

pub fn actual_reset_date(year: i32, month: u32, reset_day: u32) -> chrono::DateTime<Local> {
    let days_in_month = days_in_month(year, month);
    let (target_year, target_month, target_day) = if reset_day <= days_in_month {
        (year, month, reset_day)
    } else {
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        (next_year, next_month, 1)
    };
    let date = NaiveDate::from_ymd_opt(target_year, target_month, target_day).unwrap();
    let naive_dt = date.and_hms_opt(0, 0, 0).unwrap();
    Local.from_local_datetime(&naive_dt).single().unwrap_or_else(|| {
        Local::now()
    })
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next_month = NaiveDate::from_ymd_opt(next_year, next_month, 1).unwrap();
    let last_of_this_month = first_of_next_month.pred_opt().unwrap();
    last_of_this_month.day()
}

pub fn parse_traffic_correction_gb(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(0);
    }
    let f: f64 = s.parse().map_err(|_| format!("invalid traffic correction value: {}", raw))?;
    if f < 0.0 || f > MAX_TRAFFIC_CORRECTION_GB {
        return Err(format!("invalid traffic correction value: {}", raw));
    }
    Ok((f * 1024.0 * 1024.0 * 1024.0) as u64)
}

pub fn parse_kv_file<P: AsRef<Path>>(path: P) -> std::io::Result<HashMap<String, String>> {
    let content = fs::read_to_string(path)?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().to_string();
            let mut v = v.trim();
            if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                v = &v[1..v.len() - 1];
            }
            map.insert(k, v.to_string());
        }
    }
    Ok(map)
}
