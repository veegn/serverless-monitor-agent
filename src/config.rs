use crate::traffic::parse_kv_file;
use crate::types::*;
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

static IFACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.:\-\*\?\[\]]+$").unwrap());

pub use crate::dns::use_public_dns_resolver;

pub fn default_config() -> Config {
    Config::default()
}

pub fn normalize_binary_value(raw: &str, def: bool) -> Result<bool, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(def);
    }
    match s {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err("expected 0 or 1".to_string()),
    }
}

pub fn normalize_connection_mode(raw: &str) -> Result<String, String> {
    let s = raw.trim().to_lowercase();
    match s.as_str() {
        "" | "auto" | "wss" | "websocket" => Ok(CONNECTION_MODE_AUTO.to_string()),
        "http" | "post" => Ok(CONNECTION_MODE_HTTP.to_string()),
        _ => Err("connection_mode only supports auto or http".to_string()),
    }
}

pub fn normalize_ping_mode(raw: &str) -> Result<String, String> {
    let s = raw.trim().to_lowercase();
    match s.as_str() {
        "" | "tcp" => Ok(PING_MODE_TCP.to_string()),
        "icmp" => Ok(PING_MODE_ICMP.to_string()),
        _ => Err("ping_mode only supports tcp or icmp".to_string()),
    }
}

pub fn normalize_interface_list(raw: &str) -> Result<String, String> {
    let parts: Vec<&str> = raw.split(',').collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in parts {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        if name.len() > 64 || !IFACE_RE.is_match(name) {
            return Err(format!("invalid interface name: {:?}", name));
        }
        if seen.insert(name.to_string()) {
            out.push(name);
        }
    }
    let joined = out.join(",");
    if joined.len() > 255 {
        return Err("interface list exceeds maximum length".to_string());
    }
    Ok(joined)
}

pub fn normalize_config_intervals(cfg: &mut Config) {
    if cfg.report_interval < 1 {
        cfg.report_interval = DEFAULT_REPORT_INTERVAL_SEC;
    }
    if cfg.collect_interval > 0 && cfg.report_interval < cfg.collect_interval {
        cfg.report_interval = cfg.collect_interval;
    }
    if cfg.reset_day > 31 {
        cfg.reset_day = 1;
    }
    if cfg.config_md5.is_empty() {
        cfg.config_md5 = "none".to_string();
    }
    if let Ok(mode) = normalize_connection_mode(&cfg.connection_mode) {
        cfg.connection_mode = mode;
    } else {
        cfg.connection_mode = CONNECTION_MODE_AUTO.to_string();
    }
    if let Ok(mode) = normalize_ping_mode(&cfg.ping_mode) {
        cfg.ping_mode = mode;
    } else {
        cfg.ping_mode = PING_MODE_TCP.to_string();
    }
    cfg.update_proxy = cfg.update_proxy.trim().to_string();
}

pub fn read_config<P: AsRef<Path>>(path: P) -> std::io::Result<Config> {
    let mut cfg = default_config();
    let values = parse_kv_file(path)?;

    if let Some(v) = values.get("SERVER_ID") {
        cfg.server_id = v.clone();
    }
    if let Some(v) = values.get("SECRET") {
        cfg.secret = v.clone();
    }
    if let Some(v) = values.get("WORKER_URL") {
        cfg.worker_url = v.clone();
    }
    if let Some(v) = values.get("NAME") {
        cfg.name = v.clone();
    }
    if let Some(v) = values.get("COLLECT_INTERVAL").and_then(|v| v.parse().ok()) {
        cfg.collect_interval = v;
    }
    if let Some(v) = values.get("REPORT_INTERVAL").and_then(|v| v.parse().ok()) {
        cfg.report_interval = v;
    }
    if let Some(v) = values.get("CT_NODE") {
        cfg.ct_node = v.clone();
    }
    if let Some(v) = values.get("CU_NODE") {
        cfg.cu_node = v.clone();
    }
    if let Some(v) = values.get("CM_NODE") {
        cfg.cm_node = v.clone();
    }
    if let Some(v) = values.get("BD_NODE") {
        cfg.bd_node = v.clone();
    }
    if let Some(v) = values.get("NODE_1") {
        cfg.node_1 = v.clone();
    }
    if let Some(v) = values.get("NODE_2") {
        cfg.node_2 = v.clone();
    }
    if let Some(v) = values.get("NODE_3") {
        cfg.node_3 = v.clone();
    }
    if let Some(v) = values.get("NODE_4") {
        cfg.node_4 = v.clone();
    }
    if let Some(v) = values.get("INTERFACE") {
        cfg.interface = normalize_interface_list(v).unwrap_or_default();
    }
    if let Some(v) = values.get("RESET_DAY").and_then(|v| v.parse().ok()) {
        cfg.reset_day = v;
    }
    if let Some(v) = values.get("CONNECTION_MODE") {
        cfg.connection_mode = normalize_connection_mode(v).unwrap_or_else(|_| CONNECTION_MODE_AUTO.to_string());
    }
    if let Some(v) = values.get("PING_MODE") {
        cfg.ping_mode = normalize_ping_mode(v).unwrap_or_else(|_| PING_MODE_TCP.to_string());
    }
    if let Some(v) = values.get("AUTO_UPDATE") {
        cfg.auto_update = v == "1";
    }
    if let Some(v) = values.get("UPDATE_PROXY") {
        cfg.update_proxy = v.trim().to_string();
    }
    if let Some(v) = values.get("CONFIG_MD5") {
        cfg.config_md5 = if v.is_empty() { "none".to_string() } else { v.clone() };
    }
    normalize_config_intervals(&mut cfg);
    Ok(cfg)
}

pub fn write_config<P: AsRef<Path>>(path: P, mut cfg: Config) -> std::io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    normalize_config_intervals(&mut cfg);

    let mut content = String::new();
    let write_kv = |buf: &mut String, k: &str, v: &str| {
        buf.push_str(&format!("{}=\"{}\"\n", k, v.replace('"', "\\\"")));
    };

    write_kv(&mut content, "SERVER_ID", &cfg.server_id);
    write_kv(&mut content, "SECRET", &cfg.secret);
    write_kv(&mut content, "WORKER_URL", &cfg.worker_url);
    write_kv(&mut content, "NAME", &cfg.name);
    write_kv(&mut content, "COLLECT_INTERVAL", &cfg.collect_interval.to_string());
    write_kv(&mut content, "REPORT_INTERVAL", &cfg.report_interval.to_string());
    write_kv(&mut content, "CT_NODE", &cfg.ct_node);
    write_kv(&mut content, "CU_NODE", &cfg.cu_node);
    write_kv(&mut content, "CM_NODE", &cfg.cm_node);
    write_kv(&mut content, "BD_NODE", &cfg.bd_node);
    write_kv(&mut content, "NODE_1", &cfg.node_1);
    write_kv(&mut content, "NODE_2", &cfg.node_2);
    write_kv(&mut content, "NODE_3", &cfg.node_3);
    write_kv(&mut content, "NODE_4", &cfg.node_4);
    write_kv(&mut content, "INTERFACE", &cfg.interface);
    write_kv(&mut content, "RESET_DAY", &cfg.reset_day.to_string());
    write_kv(&mut content, "CONNECTION_MODE", &cfg.connection_mode);
    write_kv(&mut content, "PING_MODE", &cfg.ping_mode);
    write_kv(&mut content, "AUTO_UPDATE", if cfg.auto_update { "1" } else { "0" });
    write_kv(&mut content, "UPDATE_PROXY", &cfg.update_proxy);
    write_kv(&mut content, "CONFIG_MD5", if cfg.config_md5.is_empty() { "none" } else { &cfg.config_md5 });

    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path)
}

pub fn split_interface_set(raw: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    if raw.is_empty() {
        return set;
    }
    for part in raw.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    set
}
