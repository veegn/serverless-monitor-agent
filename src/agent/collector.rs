use std::time::Instant;

use crate::metrics::{
    collect_basic_stats, disk_io_stats_from_counters, read_cpu_percent, read_disk_io_counters,
    read_net_bytes, sample_metrics_to_map,
};
use crate::platform::native_arch;
use crate::storage::cache::CachedSample;
use crate::types::{Config, DiskIOStats, Metrics, NetBytes};

use super::probe_worker::{probe_loss_value, probe_rtt_value};
use super::Agent;

pub(super) fn counter_delta(prev: u64, current: u64) -> u64 {
    if current >= prev {
        current - prev
    } else {
        current
    }
}

impl Agent {
    pub(super) fn reset_network_baseline(&self, iface: &str) {
        let current = read_net_bytes(iface).unwrap_or_default();
        *self.prev_net.lock().unwrap() = current;
        *self.prev_time.lock().unwrap() = Some(Instant::now());
    }

    pub(super) fn network_rates(
        &self,
        now: Instant,
        current_res: Result<NetBytes, String>,
    ) -> (NetBytes, u64, u64) {
        let mut prev_net_guard = self.prev_net.lock().unwrap();
        let mut prev_time_guard = self.prev_time.lock().unwrap();

        let current = match current_res {
            Ok(c) => c,
            Err(e) => {
                self.log.warn_once(
                    "network-counters",
                    &format!("network counters unavailable: {}", e),
                );
                return (*prev_net_guard, 0, 0);
            }
        };

        let mut rx_speed = 0u64;
        let mut tx_speed = 0u64;

        if let Some(prev_time) = *prev_time_guard {
            let elapsed = now.duration_since(prev_time).as_secs_f64();
            if elapsed > 0.0 {
                let rx_delta = counter_delta(prev_net_guard.rx, current.rx);
                let tx_delta = counter_delta(prev_net_guard.tx, current.tx);
                rx_speed = (rx_delta as f64 / elapsed) as u64;
                tx_speed = (tx_delta as f64 / elapsed) as u64;
            }
        }

        *prev_net_guard = current;
        *prev_time_guard = Some(now);
        (current, rx_speed, tx_speed)
    }

    pub(super) fn sample_disk_io(&self, now: Instant) -> DiskIOStats {
        let devices = self.basic.read().unwrap().disk_devices.clone();
        let current = read_disk_io_counters(&devices);

        let mut prev_guard = self.prev_disk.lock().unwrap();
        let mut prev_at_guard = self.prev_disk_at.lock().unwrap();

        if prev_at_guard.is_none() {
            *prev_guard = current;
            *prev_at_guard = Some(now);
            return DiskIOStats::default();
        }

        let elapsed = now.duration_since(prev_at_guard.unwrap()).as_secs_f64();
        let stats = disk_io_stats_from_counters(prev_guard.clone(), current.clone(), elapsed);
        *prev_guard = current;
        *prev_at_guard = Some(now);
        stats
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_metrics(
        &self,
        cfg: &Config,
        cpu: &str,
        net_now: NetBytes,
        rx_speed: u64,
        tx_speed: u64,
        rx_monthly: u64,
        tx_monthly: u64,
        disk_io: DiskIOStats,
    ) -> Metrics {
        let probes = self.probes.read().unwrap().clone();
        let b = self.basic.read().unwrap().clone();

        Metrics {
            cpu: cpu.to_string(),
            ram_total: b.mem_total_mb.to_string(),
            ram_used: b.mem_used_mb.to_string(),
            swap_total: b.swap_total_mb.to_string(),
            swap_used: b.swap_used_mb.to_string(),
            disk_total: b.disk_total_mb.to_string(),
            disk_used: b.disk_used_mb.to_string(),
            disk: disk_io,
            load_avg: if b.load_avg.is_empty() {
                "0 0 0".to_string()
            } else {
                b.load_avg
            },
            boot_time: b.boot_time_ms.to_string(),
            net_rx: net_now.rx.to_string(),
            net_tx: net_now.tx.to_string(),
            net_rx_monthly: rx_monthly.to_string(),
            net_tx_monthly: tx_monthly.to_string(),
            net_in_speed: rx_speed.to_string(),
            net_out_speed: tx_speed.to_string(),
            os: if b.os_name.is_empty() {
                std::env::consts::OS.to_string()
            } else {
                b.os_name
            },
            arch: if b.arch.is_empty() {
                native_arch()
            } else {
                b.arch
            },
            kernel_version: b.kernel,
            cpu_info: if b.cpu_info.is_empty() {
                native_arch()
            } else {
                b.cpu_info
            },
            cpu_cores: b.cpu_cores.to_string(),
            gpu_info: b.gpu_info,
            processes: b.processes.to_string(),
            tcp_conn: b.tcp_conn.to_string(),
            udp_conn: b.udp_conn.to_string(),
            ip_v4: if probes.ip_v4.is_empty() {
                "0".to_string()
            } else {
                probes.ip_v4
            },
            ip_v6: if probes.ip_v6.is_empty() {
                "0".to_string()
            } else {
                probes.ip_v6
            },
            ping_ct: probe_rtt_value(&cfg.ct_node, probes.ct),
            ping_cu: probe_rtt_value(&cfg.cu_node, probes.cu),
            ping_cm: probe_rtt_value(&cfg.cm_node, probes.cm),
            ping_bd: probe_rtt_value(&cfg.bd_node, probes.bd),
            ping_node_1: probe_rtt_value(&cfg.node_1, probes.node_1),
            ping_node_2: probe_rtt_value(&cfg.node_2, probes.node_2),
            ping_node_3: probe_rtt_value(&cfg.node_3, probes.node_3),
            ping_node_4: probe_rtt_value(&cfg.node_4, probes.node_4),
            loss_ct: probe_loss_value(&cfg.ct_node, probes.ct),
            loss_cu: probe_loss_value(&cfg.cu_node, probes.cu),
            loss_cm: probe_loss_value(&cfg.cm_node, probes.cm),
            loss_bd: probe_loss_value(&cfg.bd_node, probes.bd),
            loss_node_1: probe_loss_value(&cfg.node_1, probes.node_1),
            loss_node_2: probe_loss_value(&cfg.node_2, probes.node_2),
            loss_node_3: probe_loss_value(&cfg.node_3, probes.node_3),
            loss_node_4: probe_loss_value(&cfg.node_4, probes.node_4),
        }
    }

    pub(super) fn append_sample(&self, now: Instant, metrics: &Metrics) {
        let (ts, _) = self.clock.lock().unwrap().timestamp(now);
        let sample = CachedSample {
            ts,
            metrics: sample_metrics_to_map(metrics),
        };
        if let Err(error) = self.cache.lock().unwrap().append_sample(&sample) {
            self.log.warn_once(
                "cache-write",
                &format!("could not persist metric sample: {}", error),
            );
        }
        *self.last_sample.lock().unwrap() = Some(now);
    }

    pub(super) fn collect_current_metrics(&self, now: Instant, cfg: &Config) -> Metrics {
        *self.basic.write().unwrap() = collect_basic_stats();
        *self.basic_at.lock().unwrap() = Some(now);
        let net_result = read_net_bytes(&cfg.interface);
        let net_valid = net_result.is_ok();
        let (net_now, rx_speed, tx_speed) = self.network_rates(now, net_result);
        let (usage, valid_cpu) = read_cpu_percent();
        let cpu = if valid_cpu {
            format!("{:.2}", usage)
        } else {
            "0.00".to_string()
        };
        if net_valid {
            let mut traffic = self.traffic.lock().unwrap();
            let tracker = traffic.get_or_insert_with(|| {
                crate::storage::traffic::TrafficTracker::new(&self.paths.traffic_file)
            });
            if let Err(error) = tracker.sample(
                &self.paths.traffic_file,
                net_now,
                cfg.reset_day,
                &cfg.interface,
                chrono::Local::now(),
                now,
            ) {
                self.log.warn_once(
                    "traffic-state",
                    &format!("monthly traffic checkpoint failed: {}", error),
                );
            }
            let (rx, tx) = tracker.totals();
            *self.monthly_rx.lock().unwrap() = rx;
            *self.monthly_tx.lock().unwrap() = tx;
        }
        let rx_monthly = *self.monthly_rx.lock().unwrap();
        let tx_monthly = *self.monthly_tx.lock().unwrap();
        let disk_io = self.sample_disk_io(now);
        *self.disk_io.lock().unwrap() = disk_io;
        self.build_metrics(
            cfg, &cpu, net_now, rx_speed, tx_speed, rx_monthly, tx_monthly, disk_io,
        )
    }
}
