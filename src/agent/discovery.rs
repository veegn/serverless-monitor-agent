use std::collections::HashMap;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::net::dns::use_public_dns_resolver;
use crate::net::http::post;
use crate::storage::config::write_config;
use crate::transport::TransportHost;
use crate::types::{Config, CONFIG_SCHEMA_VERSION};

use super::Agent;

pub const AGENT_AUTO_DISCOVERY_HEADER: &str = "x-sma-auto-discovery";
pub const AGENT_AUTO_DISCOVERY_SERVER_ID_KEY: &str = "x-sma-server-id";
pub const AGENT_AUTO_DISCOVERY_CREATED: &str = "created";
pub const AGENT_AUTO_DISCOVERY_REUSED: &str = "reused";
pub const AGENT_AUTO_DISCOVERY_FAILED: &str = "failed";
pub const AGENT_AUTO_DISCOVERY_DISABLED: &str = "disabled";

impl Agent {
    pub(super) fn post_report_body(
        &self,
        cfg: &Config,
        body: &[u8],
        sample_count: usize,
        fallback: bool,
    ) -> i32 {
        let label = if fallback { "POST fallback" } else { "report" };
        let req_id = format!(
            "{:012x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );
        self.log.debug(&format!(
            "{} attempt url={} payload_bytes={} samples={} req_id={}",
            label,
            cfg.worker_url,
            body.len(),
            sample_count,
            req_id
        ));

        let md5 = if cfg.config_md5.is_empty() {
            "none"
        } else {
            &cfg.config_md5
        };
        let headers = [
            ("Content-Type", "application/json"),
            ("Accept", "*/*"),
            ("User-Agent", "cfsm"),
            ("X-Agent-Config-Schema", CONFIG_SCHEMA_VERSION),
            ("X-Agent-Version", self.version.as_str()),
            ("X-Agent-Config-Md5", md5),
            ("X-Request-Id", req_id.as_str()),
        ];

        let started = Instant::now();
        let resp = match post(
            &cfg.worker_url,
            &headers,
            body,
            Duration::from_secs(8),
            use_public_dns_resolver(cfg),
        ) {
            Ok(r) => r,
            Err(e) => {
                self.log.error_once(
                    &format!("{}-net-err", label),
                    &format!(
                        "{} network request failed [req_id={}]: {}",
                        label, req_id, e
                    ),
                );
                self.log
                    .debug(&format!("{} failed: {} req_id={}", label, e, req_id));
                return 0;
            }
        };

        let received = Instant::now();
        self.handle_timed_report_response(
            resp.status as i32,
            &resp.body,
            &resp.headers,
            started,
            received,
            &req_id,
        );
        resp.status as i32
    }

    pub(super) fn handle_timed_report_response(
        &self,
        status_code: i32,
        resp_body: &[u8],
        headers: &HashMap<String, String>,
        started: Instant,
        received: Instant,
        req_id: &str,
    ) {
        let body_preview = String::from_utf8_lossy(resp_body).trim().to_string();
        self.log.debug(&format!(
            "report response http={} [req_id={}] body={}",
            status_code, req_id, body_preview
        ));

        if status_code >= 400 {
            self.log.error_once(
                &format!("http-status-{}", status_code),
                &format!(
                    "report server response error http={} [req_id={}] body={}",
                    status_code, req_id, body_preview
                ),
            );
        }

        self.apply_auto_discovery_headers(headers);

        if status_code == 404
            && headers
                .get(AGENT_AUTO_DISCOVERY_HEADER)
                .map(|s| s.trim())
                .unwrap_or("")
                .is_empty()
            && self.config_snapshot().server_id.is_empty()
        {
            self.abort_auto_discovery(AGENT_AUTO_DISCOVERY_DISABLED, "");
            return;
        }

        if !(200..300).contains(&status_code) {
            return;
        }

        self.handle_wss_runtime_headers(headers);

        if let Some(date_str) = headers.get("date") {
            let rtt = received.duration_since(started);
            self.calibrate_date(date_str, rtt);
        }

        let raw_body = String::from_utf8_lossy(resp_body).trim().to_string();
        if raw_body.is_empty() || raw_body == "{}" || raw_body.eq_ignore_ascii_case("OK") {
            return;
        }

        if status_code == 200 {
            if let Err(e) = self.apply_remote_config(resp_body, headers) {
                let mut last_key = self.last_config_reject_key.lock().unwrap();
                if *last_key != e {
                    *last_key = e.clone();
                    self.log
                        .warn(&format!("dynamic configuration rejected: {}", e));
                }
            } else {
                *self.last_config_reject_key.lock().unwrap() = String::new();
            }
        }
    }

    pub(super) fn apply_auto_discovery_headers(&self, headers: &HashMap<String, String>) {
        let raw = match headers.get(AGENT_AUTO_DISCOVERY_HEADER) {
            Some(s) => s.trim().to_ascii_lowercase(),
            None => return,
        };
        if raw.is_empty() {
            return;
        }

        let parts: Vec<&str> = raw.splitn(2, ':').collect();
        let status = parts[0];
        let code = if parts.len() > 1 { parts[1] } else { "" };

        match status {
            AGENT_AUTO_DISCOVERY_CREATED => {
                let server_id = headers
                    .get(AGENT_AUTO_DISCOVERY_SERVER_ID_KEY)
                    .map(|s| s.trim())
                    .unwrap_or("");
                if server_id.is_empty() {
                    return;
                }
                let cfg = self.config_snapshot();
                if cfg.server_id == server_id {
                    return;
                }
                let mut next_cfg = cfg;
                next_cfg.server_id = server_id.to_string();

                if let Err(e) = write_config(&self.paths.config_file, next_cfg.clone()) {
                    *self.pending_persist_id.lock().unwrap() = server_id.to_string();
                    self.log.warn(&format!(
                        "failed to persist auto discovery server id (still effective in memory, will retry): {}",
                        e
                    ));
                }
                *self.cfg.write().unwrap() = next_cfg;
                self.log.info("auto discovery created server successfully, please check dashboard after 2 minutes");
                if let Some(r) = self.reporter.lock().unwrap().as_ref() {
                    r.reset_report_interval();
                }
                self.sync_report_transport();
                self.wake_tick();
            }
            AGENT_AUTO_DISCOVERY_REUSED => {
                self.abort_auto_discovery(AGENT_AUTO_DISCOVERY_REUSED, "");
            }
            AGENT_AUTO_DISCOVERY_FAILED => {
                let detail = if code.is_empty() { "unknown" } else { code };
                self.abort_auto_discovery(AGENT_AUTO_DISCOVERY_FAILED, detail);
            }
            _ => {}
        }
    }

    pub(super) fn abort_auto_discovery(&self, kind: &str, detail: &str) {
        if self.discovery_failed.swap(true, AtomicOrdering::Relaxed) {
            return;
        }
        match kind {
            AGENT_AUTO_DISCOVERY_REUSED => {
                self.log.warn("auto discovery aborted: server with same name already exists; please retry with another -name or install with panel -id");
            }
            AGENT_AUTO_DISCOVERY_DISABLED => {
                self.log.warn("auto discovery disabled on server: please enable auto-discovery in dashboard settings");
            }
            _ => {
                self.log.warn(&format!(
                    "auto discovery failed: code={}, aborting. {}",
                    detail,
                    auto_discovery_failure_hint(detail)
                ));
            }
        }
    }

    pub(super) fn persist_pending_server_id(&self) {
        let mut pending = self.pending_persist_id.lock().unwrap();
        if pending.is_empty() {
            return;
        }
        let cfg = self.config_snapshot();
        if write_config(&self.paths.config_file, cfg).is_ok() {
            *pending = String::new();
            self.log.info(&format!(
                "auto discovery server id persisted to {}, no need to re-register after restart",
                self.paths.config_file
            ));
        }
    }
}

fn auto_discovery_failure_hint(code: &str) -> &'static str {
    match code {
        "no_name" => "server cannot obtain server name, please specify with -name=NAME or install with panel -id",
        "db_error" => "server database error during registration, please check Worker logs or install with panel -id",
        _ => "please check Worker logs or install with panel -id",
    }
}
