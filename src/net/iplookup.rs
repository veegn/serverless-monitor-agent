use crate::net::http::get;
use crate::util::log::Logger;
use regex::Regex;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;

static IPV4_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b").unwrap());

static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"([0-9A-Fa-f]{1,4}:){7}[0-9A-Fa-f]{1,4}|([0-9A-Fa-f]{1,4}:){1,6}:([0-9A-Fa-f]{1,4}:){0,4}[0-9A-Fa-f]{0,4}",
    )
    .unwrap()
});

pub const IPV4_LOOKUP_ENDPOINTS: &[&str] = &[
    "https://www.visa.cn/cdn-cgi/trace",
    "https://www.qualcomm.cn/cdn-cgi/trace",
    "https://edge-ip.html.zone/geo",
    "https://vercel-ip.html.zone/geo",
    "http://ipv4.ip.sb",
    "https://api.ipify.org?format=json",
    "https://cloudflare.com/cdn-cgi/trace",
];

pub const IPV6_LOOKUP_ENDPOINTS: &[&str] = &[
    "https://v6.ip.zxinc.org/info.php?type=json",
    "https://api6.ipify.org?format=json",
    "https://ipv6.icanhazip.com",
    "http://api-ipv6.ip.sb/geoip",
    "https://cloudflare.com/cdn-cgi/trace",
];

const IP_LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);
const IP_LOOKUP_USER_AGENT: &str = "curl/8.0.1";

pub fn lookup_public_ip(network: &str, logger: &Logger, use_public_dns: bool) -> String {
    let (endpoints, want_v4) = if network == "tcp6" {
        (IPV6_LOOKUP_ENDPOINTS, false)
    } else {
        (IPV4_LOOKUP_ENDPOINTS, true)
    };

    let headers = [("User-Agent", IP_LOOKUP_USER_AGENT)];

    for &endpoint in endpoints {
        match get(endpoint, &headers, IP_LOOKUP_TIMEOUT, use_public_dns) {
            Ok(resp) => {
                let body = resp.text();
                let ip = extract_ip_from_body(&body, want_v4);
                if !ip.is_empty() {
                    logger.debug(&format!("lookup {} via {} success: {}", network, endpoint, ip));
                    return ip;
                } else {
                    logger.debug(&format!(
                        "lookup {} via {}: no valid address in response",
                        network, endpoint
                    ));
                }
            }
            Err(err) => {
                logger.debug(&format!("lookup {} via {} failed: {}", network, endpoint, err));
            }
        }
    }
    String::new()
}

pub fn extract_ip_from_body(body: &str, want_v4: bool) -> String {
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ip=") {
            let val = rest.trim();
            if let Ok(ip) = val.parse::<IpAddr>() {
                if ip.is_ipv4() == want_v4 {
                    return ip.to_string();
                }
            }
        }
    }

    if want_v4 {
        for mat in IPV4_RE.find_iter(body) {
            if let Ok(ip) = mat.as_str().parse::<IpAddr>() {
                if ip.is_ipv4() {
                    return ip.to_string();
                }
            }
        }
    } else {
        for mat in IPV6_RE.find_iter(body) {
            if let Ok(ip) = mat.as_str().parse::<IpAddr>() {
                if ip.is_ipv6() {
                    return ip.to_string();
                }
            }
        }
    }

    String::new()
}
