//! WebSocket frames and payload decoding.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsHelloFrame {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WsServerFrame {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub persisted: Option<bool>,
    #[serde(default, rename = "nextD1WriteAfterMs")]
    pub next_d1_write_after_ms: Option<i64>,
    #[serde(default, rename = "nextWssReportAfterMs")]
    pub next_wss_report_after_ms: Option<i64>,
    #[serde(default, rename = "realtimeHint")]
    pub realtime_hint: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub connection_mode: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub config_body: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub config_md5: String,
    #[serde(default, rename = "configMd5")]
    pub config_md5_camel: String,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
}

pub fn wss_config_body_and_headers(
    frame: &WsServerFrame,
) -> Option<(Vec<u8>, HashMap<String, String>)> {
    let mut body = if !frame.body.is_empty() {
        frame.body.clone()
    } else if !frame.config_body.is_empty() {
        frame.config_body.clone()
    } else if let Some(v) = &frame.config {
        wss_string_from_value(v)
    } else {
        String::new()
    };

    let mut md5 = if !frame.config_md5.is_empty() {
        frame.config_md5.clone()
    } else if !frame.config_md5_camel.is_empty() {
        frame.config_md5_camel.clone()
    } else {
        frame.md5.clone()
    };

    let mut headers = HashMap::new();
    if let Some(h) = &frame.headers {
        if let Some(map) = h.as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    headers.insert(k.clone(), s.to_string());
                }
            }
            if let Some(h_md5) = headers.get("X-Agent-Config-Md5").or_else(|| headers.get("x-agent-config-md5")) {
                if md5.is_empty() {
                    md5 = h_md5.clone();
                }
            }
        }
    }

    if body.is_empty() {
        if let Some(p) = &frame.payload {
            let (p_body, p_md5, p_headers) = wss_config_payload(p);
            if !p_body.is_empty() {
                body = p_body;
                if md5.is_empty() {
                    md5 = p_md5;
                }
                for (k, v) in p_headers {
                    headers.insert(k, v);
                }
            }
        }
    }

    if body.is_empty() {
        if let Some(c) = &frame.config {
            let (c_body, c_md5, c_headers) = wss_config_payload(c);
            if !c_body.is_empty() {
                body = c_body;
                if md5.is_empty() {
                    md5 = c_md5;
                }
                for (k, v) in c_headers {
                    headers.insert(k, v);
                }
            }
        }
    }

    if !md5.is_empty() {
        headers.insert("X-Agent-Config-Md5".to_string(), md5);
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some((trimmed.as_bytes().to_vec(), headers))
    }
}

pub fn wss_config_payload(payload: &serde_json::Value) -> (String, String, HashMap<String, String>) {
    match payload {
        serde_json::Value::String(s) => (s.trim().to_string(), String::new(), HashMap::new()),
        serde_json::Value::Object(map) => {
            let mut headers = HashMap::new();
            if let Some(h) = map.get("headers").and_then(|v| v.as_object()) {
                for (k, v) in h {
                    if let Some(s) = v.as_str() {
                        headers.insert(k.clone(), s.to_string());
                    }
                }
            }
            let body = map
                .get("body")
                .and_then(|v| v.as_str())
                .or_else(|| map.get("config").and_then(|v| v.as_str()))
                .unwrap_or("")
                .trim()
                .to_string();
            let md5 = map
                .get("config_md5")
                .or_else(|| map.get("configMd5"))
                .or_else(|| map.get("md5"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            if !body.is_empty() {
                return (body, md5, headers);
            }

            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            let keys = [
                "collect_interval", "report_interval", "wss_report_interval", "reset_day", "schema_version",
                "interface", "connection_mode", "ping_mode", "custom_ct", "custom_cu", "custom_cm",
                "custom_bd", "node_1", "node_2", "node_3", "node_4", "rx_correction", "tx_correction", "update",
            ];
            for key in keys {
                if let Some(val) = map.get(key) {
                    let s = wss_string_from_value(val);
                    serializer.append_pair(key, &s);
                }
            }
            (serializer.finish(), md5, headers)
        }
        _ => (String::new(), String::new(), HashMap::new()),
    }
}

pub fn wss_string_from_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => if *b { "1".to_string() } else { "0".to_string() },
        _ => String::new(),
    }
}
