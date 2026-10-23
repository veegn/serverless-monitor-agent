use crate::command::command_output_with_timeout;
use crate::dns::resolve_host;
use crate::log::Logger;
use crate::types::{ProbeResult, PING_MODE_ICMP, PING_MODE_TCP};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub const DEFAULT_PING_TIMEOUT: Duration = Duration::from_millis(1500);
pub const HIGH_LATENCY_THRESHOLD_MS: i32 = 1000;
pub const PING_HIGH_LATENCY_RETRIES: usize = 3;
pub const RETRY_DROP_THRESHOLD_TCP_MS: i32 = 800;
pub const DEFAULT_TASK_PING_TCP_PORT: u16 = 80;
pub const DEFAULT_METRICS_TCP_PORT: u16 = 80;
pub const DNS_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

struct DnsCacheEntry {
    ip: IpAddr,
    expires_at: Instant,
}

static DNS_CACHE: RwLock<Option<HashMap<String, DnsCacheEntry>>> = RwLock::new(None);

pub fn split_probe_target(target: &str, default_port: u16) -> Result<(String, u16), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("empty target".to_string());
    }
    let mut host = target.to_string();
    let mut port = default_port;

    if let Ok(sa) = target.parse::<SocketAddr>() {
        host = sa.ip().to_string();
        port = sa.port();
    } else if let Some((h, p)) = target.rsplit_once(':') {
        if !h.contains(':') || (h.starts_with('[') && h.ends_with(']')) {
            if let Ok(parsed_port) = p.parse::<u16>() {
                host = h.to_string();
                port = parsed_port;
            }
        }
    }

    let host = host.trim_matches('[').trim_matches(']').to_string();
    if host.is_empty() || host.starts_with('-') {
        return Err("invalid host".to_string());
    }
    if port == 0 {
        return Err("invalid port".to_string());
    }
    Ok((host, port))
}

pub fn resolve_first_ip(host: &str) -> Result<IpAddr, String> {
    let host = host.trim().trim_matches('[').trim_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    let key = host.to_ascii_lowercase();
    let now = Instant::now();

    {
        let cache = DNS_CACHE.read().unwrap();
        if let Some(ref map) = *cache {
            if let Some(entry) = map.get(&key) {
                if now < entry.expires_at {
                    return Ok(entry.ip);
                }
            }
        }
    }

    let ips = resolve_host(host, false)?;
    if let Some(&ip) = ips.first() {
        let mut cache = DNS_CACHE.write().unwrap();
        let map = cache.get_or_insert_with(HashMap::new);
        map.insert(
            key,
            DnsCacheEntry {
                ip,
                expires_at: now + DNS_CACHE_TTL,
            },
        );
        return Ok(ip);
    }
    Err("no address".to_string())
}

pub fn tcp_ping(target: &str, default_port: u16, timeout: Duration) -> Result<i32, String> {
    let (host, port) = split_probe_target(target, default_port)?;
    let ip = resolve_first_ip(&host)?;
    let addr = SocketAddr::new(ip, port);
    let start = Instant::now();
    let _conn = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    let mut ms = start.elapsed().as_millis() as i32;
    if ms < 1 {
        ms = 1;
    }
    Ok(ms)
}

pub fn http_ping(target: &str, timeout: Duration) -> Result<i32, String> {
    let mut raw = target.trim().to_string();
    if raw.is_empty() {
        return Err("empty target".to_string());
    }
    if !raw.starts_with("http://") && !raw.starts_with("https://") {
        raw = format!("http://{}", raw);
    }
    let parsed = url::Url::parse(&raw).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("empty host")?;
    let is_https = parsed.scheme() == "https";
    let port = parsed.port().unwrap_or(if is_https { 443 } else { 80 });

    let ip = resolve_first_ip(host)?;
    let _addr = SocketAddr::new(ip, port);

    let start = Instant::now();
    let resp = crate::http::get(&parsed.to_string(), &[("User-Agent", "cfsm")], timeout, false)?;
    let mut ms = start.elapsed().as_millis() as i32;
    if ms < 1 {
        ms = 1;
    }
    if resp.status >= 200 && resp.status < 400 {
        Ok(ms)
    } else {
        Err(format!("http status {}", resp.status))
    }
}

pub fn icmp_ping(target: &str, timeout: Duration) -> Result<i32, String> {
    let (host, _) = split_probe_target(target, DEFAULT_TASK_PING_TCP_PORT)
        .unwrap_or_else(|_| (target.trim().trim_matches('[').trim_matches(']').to_string(), 0));
    if host.is_empty() {
        return Err("empty target".to_string());
    }
    let ip = resolve_first_ip(&host)?;

    // Call system ping command with timeout
    let timeout_secs = timeout.as_secs().max(1).to_string();
    let ip_str = ip.to_string();

    #[cfg(unix)]
    {
        let is_v6 = ip.is_ipv6();
        let cmd = if is_v6 { "ping6" } else { "ping" };
        let args: Vec<&str> = if cfg!(target_os = "macos") || cfg!(target_os = "freebsd") {
            vec!["-c", "1", "-t", &timeout_secs, &ip_str]
        } else {
            // Linux
            vec!["-c", "1", "-W", &timeout_secs, &ip_str]
        };

        let start = Instant::now();
        let out = command_output_with_timeout(timeout + Duration::from_millis(500), cmd, &args);
        match out {
            Ok(output) => {
                // Parse rtt min/avg/max/mdev = 1.234/...
                if let Some(rtt) = parse_ping_rtt(&output) {
                    return Ok(rtt);
                }
                let mut ms = start.elapsed().as_millis() as i32;
                if ms < 1 {
                    ms = 1;
                }
                Ok(ms)
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(windows)]
    {
        let timeout_ms = timeout.as_millis().to_string();
        let args = ["-n", "1", "-w", &timeout_ms, &ip_str];
        let start = Instant::now();
        let out = command_output_with_timeout(timeout + Duration::from_millis(500), "ping", &args);
        match out {
            Ok(output) => {
                if let Some(rtt) = parse_windows_ping_rtt(&output) {
                    return Ok(rtt);
                }
                let mut ms = start.elapsed().as_millis() as i32;
                if ms < 1 {
                    ms = 1;
                }
                Ok(ms)
            }
            Err(e) => Err(e),
        }
    }
}

fn parse_ping_rtt(output: &str) -> Option<i32> {
    for line in output.lines() {
        if line.contains("rtt min/avg/max") || line.contains("round-trip min/avg/max") {
            if let Some((_, stats)) = line.split_once('=') {
                let parts: Vec<&str> = stats.trim().split('/').collect();
                if parts.len() >= 2 {
                    if let Ok(avg) = parts[1].trim().parse::<f64>() {
                        let ms = avg.round() as i32;
                        return Some(ms.max(1));
                    }
                }
            }
        }
        // time=12.3 ms
        if line.contains("time=") {
            if let Some((_, rest)) = line.split_once("time=") {
                let time_str = rest.split_whitespace().next().unwrap_or("").trim_end_matches("ms");
                if let Ok(ms_f) = time_str.parse::<f64>() {
                    let ms = ms_f.round() as i32;
                    return Some(ms.max(1));
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn parse_windows_ping_rtt(output: &str) -> Option<i32> {
    for line in output.lines() {
        // Minimum = 10ms, Maximum = 10ms, Average = 10ms
        if line.contains("Average =") || line.contains("平均 =") {
            if let Some((_, rest)) = line.rsplit_once('=') {
                let val_str = rest.trim().trim_end_matches("ms").trim();
                if let Ok(ms) = val_str.parse::<i32>() {
                    return Some(ms.max(1));
                }
            }
        }
    }
    None
}

pub fn measure_probe(kind: &str, target: &str, count: usize, default_port: u16, logger: &Logger) -> ProbeResult {
    let target = target.trim();
    if target.is_empty() {
        return ProbeResult {
            rtt_ms: -1,
            loss: 100,
            ok: false,
        };
    }
    let kind = if kind == PING_MODE_ICMP {
        PING_MODE_ICMP
    } else {
        PING_MODE_TCP
    };
    let count = if count < 1 { 4 } else { count };
    let default_port = if default_port == 0 {
        DEFAULT_METRICS_TCP_PORT
    } else {
        default_port
    };

    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let res = if kind == PING_MODE_ICMP {
            icmp_ping(target, DEFAULT_PING_TIMEOUT)
        } else {
            tcp_ping(target, default_port, DEFAULT_PING_TIMEOUT)
        };
        match res {
            Ok(ms) => values.push(ms),
            Err(err) => {
                logger.debug(&format!("probe {} failed target={} err={}", kind, target, err));
            }
        }
    }
    build_probe_result(count, &values)
}

pub fn probe_history_key(kind: &str, target: &str) -> String {
    let kind = if kind == PING_MODE_ICMP {
        PING_MODE_ICMP
    } else {
        PING_MODE_TCP
    };
    let target = target.trim();
    if target.is_empty() {
        return String::new();
    }
    format!("{}\x00{}", kind, target)
}

pub fn build_probe_result(count: usize, values: &[i32]) -> ProbeResult {
    let count = count.max(1);
    let ok = values.len();
    if ok == 0 {
        return ProbeResult {
            rtt_ms: -1,
            loss: 100,
            ok: false,
        };
    }
    ProbeResult {
        rtt_ms: median_int(values),
        loss: ((count - ok) * 100 / count) as i32,
        ok: true,
    }
}

pub fn median_int(values: &[i32]) -> i32 {
    if values.is_empty() {
        return -1;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let len = sorted.len();
    if len % 2 == 1 {
        sorted[len / 2]
    } else {
        (sorted[len / 2 - 1] + sorted[len / 2]) / 2
    }
}

pub fn measure_ping(kind: &str, target: &str, timeout: Duration) -> Result<i32, String> {
    measure_ping_with_retries(kind, target, timeout, DEFAULT_TASK_PING_TCP_PORT)
}

pub fn measure_ping_with_retries(
    kind: &str,
    target: &str,
    timeout: Duration,
    default_port: u16,
) -> Result<i32, String> {
    retry_measured_ping(kind, || match kind {
        "icmp" => icmp_ping(target, timeout),
        "tcp" => tcp_ping(target, default_port, timeout),
        "http" => http_ping(target, timeout),
        _ => Err(format!("unsupported ping type {:?}", kind)),
    })
}

pub fn retry_measured_ping<F>(kind: &str, mut measure: F) -> Result<i32, String>
where
    F: FnMut() -> Result<i32, String>,
{
    let latency = measure()?;
    let first_latency = latency;
    if latency <= HIGH_LATENCY_THRESHOLD_MS || PING_HIGH_LATENCY_RETRIES == 0 {
        return Ok(latency);
    }
    for i in 0..PING_HIGH_LATENCY_RETRIES {
        let second = measure()?;
        if second <= HIGH_LATENCY_THRESHOLD_MS {
            if kind == "tcp" && first_latency - second > RETRY_DROP_THRESHOLD_TCP_MS {
                return Err("suspicious retransmission detected in tcp handshake".to_string());
            }
            return Ok(second);
        }
        if i == PING_HIGH_LATENCY_RETRIES - 1 {
            return Err("latency remains high after retries".to_string());
        }
    }
    Ok(latency)
}
