use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::net::dns::use_public_dns_resolver;
use crate::types::Config;
use crate::update::{
    check_latest_update, darwin_update_supported, fetch_update_binary, schedule_update_install,
    UpdateCandidate, AUTO_UPDATE_CHECK_INTERVAL, AUTO_UPDATE_DELAY, AUTO_UPDATE_LOCK_TTL,
};

use super::Agent;

impl Agent {
    pub(super) fn auto_update_worker(&self) {
        self.check_and_schedule_agent_update("startup");
        loop {
            if self.shutdown.load(AtomicOrdering::Relaxed) {
                return;
            }
            std::thread::sleep(AUTO_UPDATE_CHECK_INTERVAL);
            self.check_and_schedule_agent_update("periodic");
        }
    }

    pub(super) fn check_and_schedule_agent_update(&self, reason: &str) {
        if cfg!(target_os = "macos") {
            let (ok, ver) = darwin_update_supported();
            if !ok {
                self.log.info(&format!(
                    "auto update skipped: current builds require macOS 13 Ventura or later reason={} version={} macos={}",
                    reason, self.version, ver
                ));
                return;
            }
        }

        let cfg = self.config_snapshot();
        let use_dns = use_public_dns_resolver(&cfg);
        let candidate_opt = match check_latest_update(&self.version, use_dns) {
            Ok(c) => c,
            Err(e) => {
                self.log.info(&format!("auto update check failed reason={}: {}", reason, e));
                return;
            }
        };

        let candidate = match candidate_opt {
            Some(c) => c,
            None => {
                self.log.info(&format!(
                    "auto update checked: current version is up to date reason={} version={}",
                    reason, self.version
                ));
                return;
            }
        };

        self.schedule_agent_update(&candidate, reason, &cfg);
    }

    pub(super) fn schedule_agent_update(&self, candidate: &UpdateCandidate, reason: &str, cfg: &Config) {
        let lock_file = Path::new(&self.paths.config_dir).join("auto_update.lock");
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;

        if let Ok(data) = fs::read_to_string(&lock_file) {
            if let Ok(last) = data.trim().parse::<i64>() {
                if now_unix - last < AUTO_UPDATE_LOCK_TTL.as_secs() as i64 {
                    self.log.info("auto update already scheduled recently");
                    return;
                }
            }
        }

        let use_dns = use_public_dns_resolver(cfg);
        let bin_path = match fetch_update_binary(&self.paths.config_dir, candidate, &cfg.update_proxy, use_dns) {
            Ok(p) => p,
            Err(e) => {
                self.log.info(&format!(
                    "auto update download failed target={}: {}",
                    candidate.tag_name, e
                ));
                return;
            }
        };

        let method = match schedule_update_install(&self.paths, &bin_path, now_unix) {
            Ok(m) => m,
            Err(e) => {
                let _ = fs::remove_file(&bin_path);
                self.log.info(&format!("schedule update failed: {}", e));
                return;
            }
        };

        let _ = fs::create_dir_all(&self.paths.config_dir);
        let _ = fs::write(&lock_file, now_unix.to_string().as_bytes());
        self.log.info(&format!(
            "auto update scheduled target={} asset={} method={} reason={} delay={}s",
            candidate.tag_name,
            candidate.asset_name,
            method,
            reason,
            AUTO_UPDATE_DELAY.as_secs()
        ));
    }
}
