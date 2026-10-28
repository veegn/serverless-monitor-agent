use super::*;
use crate::types::Paths;
use crate::util::log::Logger;
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[path = "../../tests/support/websocket.rs"]
mod ws;

struct Fixture {
    root: std::path::PathBuf,
    agent: Arc<Agent>,
}
impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sma-agent-state-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let paths = Paths {
            cache_file: root.join("cache.jsonl").to_string_lossy().into_owned(),
            config_file: root.join("config").to_string_lossy().into_owned(),
            traffic_file: root.join("traffic").to_string_lossy().into_owned(),
            ..Paths::default()
        };
        let config = Config {
            server_id: "state-test".into(),
            secret: "secret".into(),
            worker_url: "http://127.0.0.1:1/update".into(),
            ..Config::default()
        };
        Self {
            root,
            agent: Arc::new(Agent::new(config, paths, Logger::new(false), "test")),
        }
    }
    fn sample(&self, ts: i64) {
        self.agent
            .cache
            .lock()
            .unwrap()
            .append_sample(&CachedSample {
                ts,
                metrics: serde_json::json!({"cpu": ts.to_string()}),
            })
            .unwrap();
    }
    fn idle_batch(&self) -> (String, usize) {
        let now = Instant::now();
        self.agent.apply_wss_mode(false, 60000, 1, false);
        self.agent
            .adaptive_reporting
            .lock()
            .unwrap()
            .next_report_due = Some(now);
        self.agent.process_adaptive_reporting(
            &self.agent.config_snapshot(),
            &Metrics::default(),
            now,
        );
        let reports = self.agent.reporting.lock().unwrap();
        let pending = reports.history.as_ref().expect("pending history report");
        (pending.report_id.clone(), pending.sample_count)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(transport) = self.agent.reporter.lock().unwrap().as_ref() {
            transport.stop("test complete");
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn history_ack_requires_matching_id_and_full_atomic_commit() {
    let f = Fixture::new();
    f.sample(1000);
    f.sample(3000);
    let (id, count) = f.idle_batch();
    for (ack_id, accepted, persisted, samples) in [
        ("unknown", true, Some(true), Some(count)),
        (id.as_str(), false, Some(true), Some(count)),
        (id.as_str(), true, Some(false), Some(count)),
        (id.as_str(), true, Some(true), Some(count - 1)),
    ] {
        f.agent
            .handle_wss_report_ack(ack_id, accepted, persisted, samples);
        assert_eq!(f.agent.cache.lock().unwrap().count(), 2);
        assert!(f.agent.reporting.lock().unwrap().history.is_some());
    }
    f.sample(5000);
    f.agent.apply_wss_mode(true, 1000, 2, false);
    assert_eq!(
        f.agent.cache.lock().unwrap().count(),
        3,
        "mode updates cannot acknowledge history"
    );
    f.agent
        .handle_wss_report_ack(&id, true, Some(true), Some(count));
    assert_eq!(f.agent.cache.lock().unwrap().peek_batch(10)[0].ts, 5000);
    f.agent
        .handle_wss_report_ack(&id, true, Some(true), Some(count));
    assert_eq!(
        f.agent.cache.lock().unwrap().count(),
        1,
        "duplicate ACK cannot consume new observations"
    );
}

#[test]
fn restart_restores_exact_history_body_and_does_not_rewrite_it_on_retry() {
    let f = Fixture::new();
    f.sample(1000);
    f.sample(3000);
    let (id, count) = f.idle_batch();
    let saved = fs::read(&f.agent.pending_report_path).unwrap();
    let original_body = f
        .agent
        .reporting
        .lock()
        .unwrap()
        .history
        .as_ref()
        .unwrap()
        .body
        .clone();
    f.agent.process_adaptive_reporting(
        &f.agent.config_snapshot(),
        &Metrics::default(),
        Instant::now() + Duration::from_secs(20),
    );
    assert_eq!(fs::read(&f.agent.pending_report_path).unwrap(), saved);
    let restarted = Agent::new(
        f.agent.config_snapshot(),
        f.agent.paths.clone(),
        Logger::new(false),
        "test",
    );
    {
        let reports = restarted.reporting.lock().unwrap();
        let pending = reports.history.as_ref().unwrap();
        assert_eq!(pending.report_id, id);
        assert_eq!(pending.body, original_body);
    }
    restarted
        .cache
        .lock()
        .unwrap()
        .append_sample(&CachedSample {
            ts: 5000,
            metrics: serde_json::json!({"cpu":"new"}),
        })
        .unwrap();
    restarted.handle_wss_report_ack(&id, true, Some(true), Some(count));
    assert_eq!(restarted.cache.lock().unwrap().peek_batch(10)[0].ts, 5000);
}

#[test]
fn realtime_progresses_while_history_waits_for_ack_and_samples_keep_accumulating() {
    let f = Fixture::new();
    f.sample(1000);
    f.sample(3000);
    f.agent.apply_wss_mode(true, 1000, 1, true);
    let now = Instant::now();
    f.agent
        .process_adaptive_reporting(&f.agent.config_snapshot(), &Metrics::default(), now);
    let (history_id, latest_id) = {
        let reports = f.agent.reporting.lock().unwrap();
        (
            reports.history.as_ref().unwrap().report_id.clone(),
            reports.latest.as_ref().unwrap().report_id.clone(),
        )
    };
    f.agent.append_sample(
        now,
        &Metrics {
            cpu: "99".into(),
            ..Metrics::default()
        },
    );
    f.agent.process_adaptive_reporting(
        &f.agent.config_snapshot(),
        &Metrics::default(),
        now + Duration::from_secs(1),
    );
    let reports = f.agent.reporting.lock().unwrap();
    assert_eq!(reports.history.as_ref().unwrap().report_id, history_id);
    assert_ne!(reports.latest.as_ref().unwrap().report_id, latest_id);
    assert_eq!(f.agent.cache.lock().unwrap().count(), 3);
}

#[test]
fn oversized_history_is_resized_and_ack_removes_only_its_original_samples() {
    let f = Fixture::new();
    for index in 1..=50 {
        f.sample(index * 2000);
    }
    let (old_id, _) = f.idle_batch();
    f.agent.shrink_pending_report();
    let (id, count) = {
        let reports = f.agent.reporting.lock().unwrap();
        let pending = reports.history.as_ref().unwrap();
        assert_ne!(pending.report_id, old_id);
        assert_eq!(pending.sample_count, 30);
        assert_eq!(pending.source_samples.len(), 50);
        assert_eq!(pending.payload["samples"][0]["ts"], 42000);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&pending.body).unwrap(),
            pending.payload
        );
        (pending.report_id.clone(), pending.sample_count)
    };
    f.sample(102000);
    f.agent
        .handle_wss_report_ack(&old_id, true, Some(true), Some(50));
    assert_eq!(f.agent.cache.lock().unwrap().count(), 51);
    f.agent
        .handle_wss_report_ack(&id, true, Some(true), Some(count));
    assert_eq!(f.agent.cache.lock().unwrap().peek_batch(10)[0].ts, 102000);
}

#[test]
fn pending_slot_remains_occupied_during_concurrent_ack_cleanup() {
    let f = Fixture::new();
    f.sample(1000);
    let (id, count) = f.idle_batch();
    let cache = f.agent.cache.lock().unwrap();
    let agent = f.agent.clone();
    let ack =
        std::thread::spawn(move || agent.handle_wss_report_ack(&id, true, Some(true), Some(count)));
    ws::wait_until(|| f.agent.reporting.try_lock().is_err());
    cache
        .append_sample(&CachedSample {
            ts: 3000,
            metrics: serde_json::json!({"cpu":"new"}),
        })
        .unwrap();
    let agent = f.agent.clone();
    let report = std::thread::spawn(move || {
        agent.process_adaptive_reporting(
            &agent.config_snapshot(),
            &Metrics::default(),
            Instant::now(),
        )
    });
    drop(cache);
    ack.join().unwrap();
    report.join().unwrap();
    assert_eq!(f.agent.cache.lock().unwrap().peek_batch(10)[0].ts, 3000);
}

#[test]
fn registration_starts_real_transport_for_ok_empty_and_object_responses() {
    for response in [b"OK".as_slice(), b"", b"{}"] {
        let f = Fixture::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        {
            let mut cfg = f.agent.cfg.write().unwrap();
            cfg.server_id.clear();
            cfg.worker_url = format!("http://{}/update", listener.local_addr().unwrap());
        }
        let transport = crate::transport::ReportTransport::new(f.agent.clone());
        *f.agent.reporter.lock().unwrap() = Some(transport.clone());
        let (finish, finished) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut stream = ws::accept(&listener);
            ws::hello(&mut stream, true, 1000);
            finished.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            super::super::AGENT_AUTO_DISCOVERY_HEADER.to_string(),
            "created".to_string(),
        );
        headers.insert(
            super::super::AGENT_AUTO_DISCOVERY_SERVER_ID_KEY.to_string(),
            "registered".to_string(),
        );
        let now = Instant::now();
        f.agent.handle_timed_report_response(
            200,
            response,
            &headers,
            now,
            now,
            "registration-test",
        );
        ws::wait_until(|| transport.connected());
        assert_eq!(f.agent.config_snapshot().server_id, "registered");
        *f.agent.next_sample_due.lock().unwrap() = Instant::now() + Duration::from_secs(2);
        f.agent.tick();
        let wait = f.agent.tick_interval();
        assert!(
            wait <= Duration::from_secs(1),
            "negotiated one-second reports must not wait two seconds"
        );
        transport.stop("test complete");
        finish.send(()).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn history_contains_dynamic_disk_network_and_probe_metrics() {
    let metrics = Metrics {
        disk_total: "100".into(),
        load_avg: "1 2 3".into(),
        tcp_conn: "7".into(),
        ping_ct: serde_json::json!("12"),
        ..Metrics::default()
    };
    let history = crate::metrics::sample_metrics_to_map(&metrics);
    for key in [
        "disk",
        "disk_total",
        "load_avg",
        "tcp_conn",
        "net_rx",
        "ping_ct",
        "loss_ct",
        "processes",
    ] {
        assert!(history.get(key).is_some(), "missing historical field {key}");
    }
    assert!(
        history.get("os").is_none(),
        "static host identity is not duplicated"
    );
}

#[test]
fn real_agent_rejects_old_mode_versions_and_does_not_reschedule_on_late_history_ack() {
    let f = Fixture::new();
    f.sample(1000);
    let (id, count) = f.idle_batch();
    assert!(f.agent.apply_wss_mode(true, 1000, 2, false));
    assert!(!f.agent.apply_wss_mode(false, 60000, 1, false));
    let next = f.agent.adaptive_reporting.lock().unwrap().next_report_due;
    f.agent
        .handle_wss_report_ack(&id, true, Some(true), Some(count));
    let state = f.agent.adaptive_reporting.lock().unwrap();
    assert_eq!(state.mode, AdaptiveMode::Realtime);
    assert_eq!(state.interval, Duration::from_secs(1));
    assert_eq!(state.next_report_due, next);
}

#[test]
fn offline_sampling_is_saved_before_any_report_is_built() {
    let f = Fixture::new();
    f.agent.tick();
    assert_eq!(f.agent.cache.lock().unwrap().count(), 1);
    assert!(f.agent.reporting.lock().unwrap().history.is_none());
    let loaded = crate::storage::cache::MetricsCache::new(&f.agent.paths.cache_file);
    assert_eq!(loaded.count(), 1);
    assert!(loaded.peek_batch(1)[0].metrics.get("disk").is_some());
}

#[test]
fn oversized_latest_frame_does_not_cause_a_busy_retry_loop() {
    let f = Fixture::new();
    f.agent.apply_wss_mode(true, 1000, 1, true);
    let now = Instant::now();
    let metrics = Metrics {
        gpu_info: Some(serde_json::json!("x".repeat(MAX_REPORT_FRAME_BYTES))),
        ..Metrics::default()
    };
    f.agent
        .process_adaptive_reporting(&f.agent.config_snapshot(), &metrics, now);
    assert!(f.agent.reporting.lock().unwrap().latest.is_none());
    assert!(f.agent.next_reporting_deadline().unwrap() >= now + Duration::from_secs(2));
}
