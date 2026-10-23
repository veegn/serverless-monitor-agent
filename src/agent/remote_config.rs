use std::collections::HashMap;
use std::time::Duration;

use regex::Regex;

use crate::net::dns::use_public_dns_resolver;
use crate::net::http::post;
use crate::storage::config::{
    normalize_connection_mode, normalize_interface_list, normalize_ping_mode, write_config,
};
use crate::storage::traffic::{apply_traffic_correction_from_network, parse_traffic_correction_gb};
use crate::types::{
    Config, CONFIG_SCHEMA_VERSION, CONNECTION_MODE_AUTO, DEFAULT_WSS_REPORT_INTERVAL_SEC,
    DiskIOCounters, DiskIOStats, MAX_WSS_REPORT_INTERVAL_SEC, MIN_WSS_REPORT_INTERVAL_SEC,
};

use super::Agent;

pub fn valid_config_md5(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn remote_config_differs(
    cfg: &Config,
    values: &HashMap<String, String>,
    collect: i32,
    report: i32,
    reset: i32,
    iface: &str,
    connection_mode: &str,
    ping_mode: &str,
) -> bool {
    (cfg.collect_interval as i64) != (collect as i64)
        || (cfg.report_interval as i64) != (report as i64)
        || (cfg.reset_day as i64) != (reset as i64)
        || cfg.ct_node != values.get("custom_ct").map(|s| s.as_str()).unwrap_or("")
        || cfg.cu_node != values.get("custom_cu").map(|s| s.as_str()).unwrap_or("")
        || cfg.cm_node != values.get("custom_cm").map(|s| s.as_str()).unwrap_or("")
        || cfg.bd_node != values.get("custom_bd").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_1 != values.get("node_1").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_2 != values.get("node_2").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_3 != values.get("node_3").map(|s| s.as_str()).unwrap_or("")
        || cfg.node_4 != values.get("node_4").map(|s| s.as_str()).unwrap_or("")
        || cfg.interface != iface
        || cfg.connection_mode != connection_mode
        || cfg.ping_mode != ping_mode
}

impl Agent {
    pub fn apply_remote_config(&self, body: &[u8], headers: &HashMap<String, String>) -> Result<(), String> {
        self.apply_remote_config_with_options(body, headers, false)
    }

    pub fn apply_remote_config_with_options(
        &self,
        body: &[u8],
        headers: &HashMap<String, String>,
        allow_missing_md5: bool,
    ) -> Result<(), String> {
        if body.is_empty() {
            return Err("empty body".to_string());
        }
        if body.len() > 1024 {
            return Err("response too large".to_string());
        }
        let raw = String::from_utf8_lossy(body).trim().to_string();
        if raw.is_empty() {
            return Err("empty body".to_string());
        }

        let re = Regex::new(r"^[A-Za-z0-9_=&.,:%+\-\*\?\[\]]*$").unwrap();
        if !re.is_match(&raw) {
            return Err("invalid body characters".to_string());
        }

        let parsed_pairs = url::form_urlencoded::parse(raw.as_bytes());
        let mut values: HashMap<String, String> = HashMap::new();
        for (k, v) in parsed_pairs {
            values.insert(k.to_string(), v.to_string());
        }

        let allowed = [
            "collect_interval", "report_interval", "wss_report_interval", "reset_day",
            "schema_version", "custom_ct", "custom_cu", "custom_cm", "custom_bd",
            "node_1", "node_2", "node_3", "node_4", "interface", "connection_mode",
            "ping_mode", "rx_correction", "tx_correction", "update",
        ];
        for k in values.keys() {
            if !allowed.contains(&k.as_str()) {
                return Err(format!("unknown field {}", k));
            }
        }

        if let Some(upd) = values.get("update") {
            if !upd.is_empty() && upd != "0" && upd != "1" {
                return Err(format!("invalid update {}", upd));
            }
        }

        let has_config = values.contains_key("collect_interval")
            || values.contains_key("report_interval")
            || values.contains_key("wss_report_interval")
            || values.contains_key("reset_day")
            || values.contains_key("schema_version")
            || values.contains_key("interface")
            || values.contains_key("connection_mode")
            || values.contains_key("ping_mode")
            || values.contains_key("node_1")
            || values.contains_key("node_2")
            || values.contains_key("node_3")
            || values.contains_key("node_4");

        let has_correction = values.contains_key("rx_correction") || values.contains_key("tx_correction");
        let cfg = self.config_snapshot();

        if !has_config {
            if has_correction {
                let rx = values.get("rx_correction").cloned().unwrap_or_default();
                let tx = values.get("tx_correction").cloned().unwrap_or_default();
                apply_traffic_correction_from_network(&self.paths.traffic_file, &cfg.interface, &rx, &tx)?;
                let _ = self.send_correction_confirm(&cfg, &rx, &tx);
                return Ok(());
            }
            if values.contains_key("update") {
                return Ok(());
            }
            return Err("no config fields".to_string());
        }

        let new_md5 = headers
            .get("X-Agent-Config-Md5")
            .or_else(|| headers.get("x-agent-config-md5"))
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let has_remote_md5 = valid_config_md5(&new_md5);

        if !has_remote_md5 && !allow_missing_md5 {
            return Err("invalid remote md5".to_string());
        }

        let collect = values.get("collect_interval").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1);
        let report = values.get("report_interval").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1);
        let wss_report = values.get("wss_report_interval").and_then(|s| s.parse::<i32>().ok()).unwrap_or(DEFAULT_WSS_REPORT_INTERVAL_SEC as i32);
        let reset = values.get("reset_day").and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1);

        if ![0, 1, 2, 5, 10].contains(&collect) {
            return Err(format!("invalid collect_interval {}", collect));
        }
        if ![30, 60, 120, 180].contains(&report) {
            return Err(format!("invalid report_interval {}", report));
        }
        if !(MIN_WSS_REPORT_INTERVAL_SEC as i32..=MAX_WSS_REPORT_INTERVAL_SEC as i32).contains(&wss_report) {
            return Err(format!("invalid wss_report_interval {}", wss_report));
        }
        if !(0..=31).contains(&reset) {
            return Err(format!("invalid reset_day {}", reset));
        }
        if values.get("schema_version").map(|s| s.as_str()) != Some(CONFIG_SCHEMA_VERSION) {
            return Err(format!("invalid schema_version {:?}", values.get("schema_version")));
        }
        if report < collect {
            return Err("report_interval less than collect_interval".to_string());
        }

        let iface = normalize_interface_list(values.get("interface").map(|s| s.as_str()).unwrap_or(""))?;
        let connection_mode = normalize_connection_mode(values.get("connection_mode").map(|s| s.as_str()).unwrap_or(""))?;
        let ping_mode = normalize_ping_mode(values.get("ping_mode").map(|s| s.as_str()).unwrap_or(""))?;

        let mut effective_collect = collect;
        if connection_mode == CONNECTION_MODE_AUTO && (collect == 0 || collect > wss_report) {
            effective_collect = wss_report;
        }

        let should_apply = if has_remote_md5 {
            new_md5 != cfg.config_md5
        } else {
            remote_config_differs(&cfg, &values, effective_collect, report, reset, &iface, &connection_mode, &ping_mode)
        };

        if should_apply {
            let mut next_cfg = cfg.clone();
            next_cfg.collect_interval = effective_collect as u64;
            next_cfg.report_interval = report as u64;
            next_cfg.reset_day = reset as u32;
            if let Some(v) = values.get("custom_ct") { next_cfg.ct_node = v.clone(); }
            if let Some(v) = values.get("custom_cu") { next_cfg.cu_node = v.clone(); }
            if let Some(v) = values.get("custom_cm") { next_cfg.cm_node = v.clone(); }
            if let Some(v) = values.get("custom_bd") { next_cfg.bd_node = v.clone(); }
            if let Some(v) = values.get("node_1") { next_cfg.node_1 = v.clone(); }
            if let Some(v) = values.get("node_2") { next_cfg.node_2 = v.clone(); }
            if let Some(v) = values.get("node_3") { next_cfg.node_3 = v.clone(); }
            if let Some(v) = values.get("node_4") { next_cfg.node_4 = v.clone(); }
            next_cfg.interface = iface.clone();
            next_cfg.connection_mode = connection_mode;
            next_cfg.ping_mode = ping_mode;
            if has_remote_md5 {
                next_cfg.config_md5 = new_md5.clone();
            }

            write_config(&self.paths.config_file, next_cfg.clone()).map_err(|e| e.to_string())?;
            *self.cfg.write().unwrap() = next_cfg.clone();

            self.reset_network_baseline(&next_cfg.interface);
            *self.prev_disk.lock().unwrap() = DiskIOCounters::default();
            *self.prev_disk_at.lock().unwrap() = None;
            *self.disk_io.lock().unwrap() = DiskIOStats::default();
            *self.monthly_rx.lock().unwrap() = 0;
            *self.monthly_tx.lock().unwrap() = 0;
            *self.full_at.lock().unwrap() = None;
            self.samples.lock().unwrap().clear();
            *self.last_sample.lock().unwrap() = None;
            *self.last_report.lock().unwrap() = None;
            *self.last_config_state_report_at.lock().unwrap() = None;
            *self.last_config_state_report_md5.lock().unwrap() = String::new();

            if let Some(r) = self.reporter.lock().unwrap().as_ref() {
                r.reset_report_interval();
            }
            self.sync_report_transport();
            self.wake_tick();

            self.log.info(&format!(
                "dynamic configuration applied md5={} connection_mode={} ping_mode={} interface={}",
                if next_cfg.config_md5.is_empty() { "none" } else { &next_cfg.config_md5 },
                next_cfg.connection_mode,
                next_cfg.ping_mode,
                if iface.is_empty() { "auto" } else { &iface }
            ));
        }

        if has_correction {
            let rx = values.get("rx_correction").cloned().unwrap_or_default();
            let tx = values.get("tx_correction").cloned().unwrap_or_default();
            apply_traffic_correction_from_network(&self.paths.traffic_file, &cfg.interface, &rx, &tx)?;
            let _ = self.send_correction_confirm(&cfg, &rx, &tx);
        }

        Ok(())
    }

    pub(super) fn send_correction_confirm(&self, cfg: &Config, rx: &str, tx: &str) -> Result<(), String> {
        let _ = parse_traffic_correction_gb(rx)?;
        let _ = parse_traffic_correction_gb(tx)?;
        let rx_val: f64 = rx.trim().parse().unwrap_or(0.0);
        let tx_val: f64 = tx.trim().parse().unwrap_or(0.0);

        let payload = serde_json::json!({
            "id": cfg.server_id,
            "secret": cfg.secret,
            "rx_correction": rx_val,
            "tx_correction": tx_val,
        });

        let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        let headers = [
            ("Content-Type", "application/json"),
            ("Accept", "*/*"),
            ("User-Agent", "cfsm"),
        ];

        let resp = post(
            &cfg.worker_url,
            &headers,
            &body,
            Duration::from_secs(4),
            use_public_dns_resolver(cfg),
        )?;
        if !(200..300).contains(&resp.status) {
            return Err(format!("http {}", resp.status));
        }
        self.log.info(&format!(
            "traffic correction confirm sent rx={}GB tx={}GB",
            if rx.is_empty() { "0" } else { rx },
            if tx.is_empty() { "0" } else { tx }
        ));
        Ok(())
    }
}
