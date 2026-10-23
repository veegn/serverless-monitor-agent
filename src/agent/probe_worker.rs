use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use crate::net::dns::use_public_dns_resolver;
use crate::net::iplookup::lookup_public_ip;
use crate::net::probe::{measure_probe, median_int};
use crate::types::{ProbeResult, ProbeSnapshot};

use super::Agent;

pub const METRICS_PROBE_INTERVAL: Duration = Duration::from_secs(20);
pub const METRICS_PROBE_MEDIAN_WINDOW: Duration = Duration::from_secs(120);
pub const METRICS_PROBE_WINDOW_SAMPLE_COUNT: usize = 6;
pub const METRICS_PROBE_SAMPLE_COUNT: usize = 1;

#[derive(Debug, Clone)]
struct TimedProbeResult {
    at: Instant,
    result: ProbeResult,
}

#[derive(Debug, Clone, Default)]
struct RollingProbeHistory {
    target: String,
    samples: Vec<TimedProbeResult>,
}

impl RollingProbeHistory {
    fn add(&mut self, now: Instant, target: &str, result: ProbeResult) {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            self.target.clear();
            self.samples.clear();
            return;
        }
        if self.target != trimmed {
            self.target = trimmed.to_string();
            self.samples.clear();
        }
        self.samples.push(TimedProbeResult { at: now, result });
        if self.samples.len() > METRICS_PROBE_WINDOW_SAMPLE_COUNT {
            let excess = self.samples.len() - METRICS_PROBE_WINDOW_SAMPLE_COUNT;
            self.samples.drain(0..excess);
        }
    }

    fn snapshot(&self, now: Instant) -> ProbeResult {
        if self.samples.is_empty() {
            return ProbeResult::default();
        }
        let cutoff = now.checked_sub(METRICS_PROBE_MEDIAN_WINDOW).unwrap_or(now);
        let mut values = Vec::new();
        for sample in &self.samples {
            if sample.at >= cutoff && sample.result.ok && sample.result.rtt_ms >= 0 {
                values.push(sample.result.rtt_ms);
            }
        }

        let loss_samples = if self.samples.len() > METRICS_PROBE_WINDOW_SAMPLE_COUNT {
            &self.samples[self.samples.len() - METRICS_PROBE_WINDOW_SAMPLE_COUNT..]
        } else {
            &self.samples[..]
        };

        let mut lost = 0;
        for sample in loss_samples {
            if !sample.result.ok {
                lost += 1;
            }
        }
        let loss = if !loss_samples.is_empty() {
            lost * 100 / loss_samples.len() as i32
        } else {
            0
        };

        if values.is_empty() {
            ProbeResult {
                rtt_ms: -1,
                loss,
                ok: false,
            }
        } else {
            ProbeResult {
                rtt_ms: median_int(&values),
                loss,
                ok: true,
            }
        }
    }
}

pub(super) fn probe_history_key(ping_mode: &str, node: &str) -> String {
    format!("{}:{}", ping_mode, node.trim())
}

pub(super) fn probe_rtt_value(node: &str, r: ProbeResult) -> serde_json::Value {
    if node.trim().is_empty() {
        serde_json::json!(false)
    } else if !r.ok || r.rtt_ms < 0 {
        serde_json::json!("null")
    } else {
        serde_json::json!(r.rtt_ms.to_string())
    }
}

pub(super) fn probe_loss_value(node: &str, r: ProbeResult) -> serde_json::Value {
    if node.trim().is_empty() {
        serde_json::json!(false)
    } else if r.loss < 0 {
        serde_json::json!("100")
    } else {
        serde_json::json!(r.loss.to_string())
    }
}

impl Agent {
    pub(super) fn network_worker(&self) {
        let mut last_ip: Option<Instant> = None;
        let mut last_probe: Option<Instant> = None;

        let mut ct_hist = RollingProbeHistory::default();
        let mut cu_hist = RollingProbeHistory::default();
        let mut cm_hist = RollingProbeHistory::default();
        let mut bd_hist = RollingProbeHistory::default();
        let mut node1_hist = RollingProbeHistory::default();
        let mut node2_hist = RollingProbeHistory::default();
        let mut node3_hist = RollingProbeHistory::default();
        let mut node4_hist = RollingProbeHistory::default();

        loop {
            if self.shutdown.load(AtomicOrdering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_secs(5));
            let now = Instant::now();
            let cfg = self.config_snapshot();

            let mut snap = ProbeSnapshot::default();
            let mut need_update = false;

            if last_ip.is_none() || now.duration_since(last_ip.unwrap()) >= Duration::from_secs(600) {
                let use_dns = use_public_dns_resolver(&cfg);
                snap.ip_v4 = lookup_public_ip("tcp4", &self.log, use_dns);
                snap.ip_v6 = lookup_public_ip("tcp6", &self.log, use_dns);
                last_ip = Some(now);
                need_update = true;
            }

            if last_probe.is_none() || now.duration_since(last_probe.unwrap()) >= METRICS_PROBE_INTERVAL {
                ct_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.ct_node), measure_probe(&cfg.ping_mode, &cfg.ct_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                cu_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.cu_node), measure_probe(&cfg.ping_mode, &cfg.cu_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                cm_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.cm_node), measure_probe(&cfg.ping_mode, &cfg.cm_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                bd_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.bd_node), measure_probe(&cfg.ping_mode, &cfg.bd_node, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node1_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_1), measure_probe(&cfg.ping_mode, &cfg.node_1, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node2_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_2), measure_probe(&cfg.ping_mode, &cfg.node_2, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node3_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_3), measure_probe(&cfg.ping_mode, &cfg.node_3, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));
                node4_hist.add(now, &probe_history_key(&cfg.ping_mode, &cfg.node_4), measure_probe(&cfg.ping_mode, &cfg.node_4, METRICS_PROBE_SAMPLE_COUNT, 80, &self.log));

                snap.ct = ct_hist.snapshot(now);
                snap.cu = cu_hist.snapshot(now);
                snap.cm = cm_hist.snapshot(now);
                snap.bd = bd_hist.snapshot(now);
                snap.node_1 = node1_hist.snapshot(now);
                snap.node_2 = node2_hist.snapshot(now);
                snap.node_3 = node3_hist.snapshot(now);
                snap.node_4 = node4_hist.snapshot(now);

                last_probe = Some(now);
                need_update = true;
            }

            if need_update {
                let mut probes = self.probes.write().unwrap();
                if snap.ip_v4.is_empty() { snap.ip_v4 = probes.ip_v4.clone(); }
                if snap.ip_v6.is_empty() { snap.ip_v6 = probes.ip_v6.clone(); }
                if snap.ct == ProbeResult::default() { snap.ct = probes.ct; }
                if snap.cu == ProbeResult::default() { snap.cu = probes.cu; }
                if snap.cm == ProbeResult::default() { snap.cm = probes.cm; }
                if snap.bd == ProbeResult::default() { snap.bd = probes.bd; }
                if snap.node_1 == ProbeResult::default() { snap.node_1 = probes.node_1; }
                if snap.node_2 == ProbeResult::default() { snap.node_2 = probes.node_2; }
                if snap.node_3 == ProbeResult::default() { snap.node_3 = probes.node_3; }
                if snap.node_4 == ProbeResult::default() { snap.node_4 = probes.node_4; }
                *probes = snap;
            }
        }
    }
}
