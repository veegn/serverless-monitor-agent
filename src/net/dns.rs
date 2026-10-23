use crate::types::Config;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

pub const DEFAULT_UPDATE_DNS_SERVERS: &[&str] = &[
    "223.5.5.5:53",
    "119.29.29.29:53",
    "114.114.114.114:53",
    "1.1.1.1:53",
    "8.8.8.8:53",
    "8.8.4.4:53",
    "[2606:4700:4700::1111]:53",
    "[2001:4860:4860::8888]:53",
];

const UPDATE_DNS_SERVER_ENV: &str = "CF_PROBE_UPDATE_DNS";

pub fn use_public_dns_resolver(cfg: &Config) -> bool {
    if std::env::var(UPDATE_DNS_SERVER_ENV).map(|v| !v.trim().is_empty()).unwrap_or(false) {
        return true;
    }
    !cfg.update_proxy.trim().is_empty()
}

pub fn normalize_update_dns_server(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('[') && s.contains("]:")) || (s.matches(':').count() == 1 && !s.contains(']')) {
        return s.to_string();
    }
    if s.matches(':').count() >= 2 && !s.contains(']') {
        return format!("[{}]:53", s);
    }
    if !s.contains(':') {
        return format!("{}:53", s);
    }
    s.to_string()
}

pub fn update_resolver_servers() -> Vec<String> {
    let mut servers = Vec::new();
    if let Ok(custom) = std::env::var(UPDATE_DNS_SERVER_ENV) {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            servers.push(normalize_update_dns_server(trimmed));
        }
    }
    for &s in DEFAULT_UPDATE_DNS_SERVERS {
        servers.push(s.to_string());
    }
    servers
}

pub fn resolve_host(host: &str, use_public_dns: bool) -> Result<Vec<IpAddr>, String> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }

    if use_public_dns {
        if let Ok(ips) = query_public_dns(host) {
            if !ips.is_empty() {
                let mut ips = ips;
                sort_update_ips_by_preference(&mut ips);
                return Ok(ips);
            }
        }
    }

    // System DNS fallback
    let addr_str = format!("{}:80", host);
    match addr_str.to_socket_addrs() {
        Ok(iter) => {
            let mut ips: Vec<IpAddr> = iter.map(|sa| sa.ip()).collect();
            if ips.is_empty() {
                Err(format!("no addresses resolved for {}", host))
            } else {
                sort_update_ips_by_preference(&mut ips);
                Ok(ips)
            }
        }
        Err(e) => Err(format!("failed to resolve {}: {}", host, e)),
    }
}

pub fn sort_update_ips_by_preference(ips: &mut [IpAddr]) {
    let prefer_v4 = prefer_update_ipv4_first();
    ips.sort_by_key(|ip| if ip.is_ipv4() == prefer_v4 { 0 } else { 1 });
}

fn prefer_update_ipv4_first() -> bool {
    // If we can bind a UDP socket to 0.0.0.0, we have an IPv4 stack.
    // Or check if local addresses have an IPv4.
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("1.1.1.1:80").is_ok() {
            return true;
        }
    }
    true
}

fn query_public_dns(host: &str) -> Result<Vec<IpAddr>, String> {
    let servers = update_resolver_servers();
    let mut last_err = String::from("no DNS servers");
    for server_str in servers {
        let server_addr: SocketAddr = match server_str.to_socket_addrs() {
            Ok(mut it) => match it.next() {
                Some(sa) => sa,
                None => continue,
            },
            Err(_) => continue,
        };

        let bind_addr = if server_addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = match UdpSocket::bind(bind_addr) {
            Ok(s) => s,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        let _ = socket.set_read_timeout(Some(Duration::from_secs(4)));
        let _ = socket.set_write_timeout(Some(Duration::from_secs(4)));

        let mut results = Vec::new();
        // Query A
        if let Ok(pkt) = build_dns_query(host, 1) {
            if socket.send_to(&pkt, server_addr).is_ok() {
                let mut buf = [0u8; 1024];
                if let Ok((n, _)) = socket.recv_from(&mut buf) {
                    if let Ok(ips) = parse_dns_response(&buf[..n], 1) {
                        results.extend(ips);
                    }
                }
            }
        }
        // Query AAAA
        if let Ok(pkt) = build_dns_query(host, 28) {
            if socket.send_to(&pkt, server_addr).is_ok() {
                let mut buf = [0u8; 1024];
                if let Ok((n, _)) = socket.recv_from(&mut buf) {
                    if let Ok(ips) = parse_dns_response(&buf[..n], 28) {
                        results.extend(ips);
                    }
                }
            }
        }

        if !results.is_empty() {
            return Ok(results);
        }
    }
    Err(last_err)
}

fn build_dns_query(host: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let mut pkt = Vec::with_capacity(64);
    // Transaction ID
    pkt.extend_from_slice(&[0x12, 0x34]);
    // Flags: standard query, recursion desired (0x0100)
    pkt.extend_from_slice(&[0x01, 0x00]);
    // Questions: 1
    pkt.extend_from_slice(&[0x00, 0x01]);
    // Answer RRs: 0
    pkt.extend_from_slice(&[0x00, 0x00]);
    // Authority RRs: 0
    pkt.extend_from_slice(&[0x00, 0x00]);
    // Additional RRs: 0
    pkt.extend_from_slice(&[0x00, 0x00]);

    // Name
    for part in host.split('.') {
        if part.is_empty() {
            continue;
        }
        if part.len() > 63 {
            return Err("domain label too long".to_string());
        }
        pkt.push(part.len() as u8);
        pkt.extend_from_slice(part.as_bytes());
    }
    pkt.push(0);

    // QType
    pkt.extend_from_slice(&qtype.to_be_bytes());
    // QClass: IN (1)
    pkt.extend_from_slice(&1u16.to_be_bytes());

    Ok(pkt)
}

fn parse_dns_response(buf: &[u8], expected_qtype: u16) -> Result<Vec<IpAddr>, String> {
    if buf.len() < 12 {
        return Err("response packet too short".to_string());
    }
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);
    if ancount == 0 {
        return Ok(Vec::new());
    }

    let mut pos = 12;
    // Skip question section
    pos = skip_name(buf, pos)?;
    if pos + 4 > buf.len() {
        return Err("truncated question".to_string());
    }
    pos += 4; // QType + QClass

    let mut ips = Vec::new();
    for _ in 0..ancount {
        if pos >= buf.len() {
            break;
        }
        pos = skip_name(buf, pos)?;
        if pos + 10 > buf.len() {
            break;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let _rclass = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
        let _ttl = u32::from_be_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]);
        let rdlength = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlength > buf.len() {
            break;
        }
        if rtype == expected_qtype {
            if rtype == 1 && rdlength == 4 {
                let octets = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
                ips.push(IpAddr::V4(Ipv4Addr::from(octets)));
            } else if rtype == 28 && rdlength == 16 {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&buf[pos..pos + 16]);
                ips.push(IpAddr::V6(Ipv6Addr::from(octets)));
            }
        }
        pos += rdlength;
    }
    Ok(ips)
}

fn skip_name(buf: &[u8], mut pos: usize) -> Result<usize, String> {
    while pos < buf.len() {
        let len = buf[pos];
        if len == 0 {
            return Ok(pos + 1);
        }
        if len & 0xc0 == 0xc0 {
            // Pointer
            return Ok(pos + 2);
        }
        pos += 1 + len as usize;
    }
    Err("invalid dns name".to_string())
}
