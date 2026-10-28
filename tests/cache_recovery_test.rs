use serverless_monitor_agent::storage::cache::{CachedSample, MetricsCache};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempCache(std::path::PathBuf);
impl TempCache {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sma-cache-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root.join("cache"))
    }
    fn cache(&self) -> MetricsCache {
        MetricsCache::new(self.0.to_str().unwrap())
    }
}
impl Drop for TempCache {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.parent().unwrap());
    }
}
fn sample(ts: i64) -> CachedSample {
    CachedSample {
        ts,
        metrics: serde_json::json!({"cpu": ts}),
    }
}

#[test]
fn invalid_lines_and_truncated_tail_do_not_poison_the_queue() {
    let f = TempCache::new();
    let first = serde_json::to_string(&sample(1000)).unwrap();
    let second = serde_json::to_string(&sample(3000)).unwrap();
    fs::write(
        &f.0,
        format!("broken\n{first}\n{{invalid}}\n{second}\n{{\"ts\":5000"),
    )
    .unwrap();
    let cache = f.cache();
    assert_eq!(cache.count(), 2);
    assert_eq!(
        cache.peek_batch_checked(50).unwrap(),
        vec![sample(1000), sample(3000)]
    );
    assert_eq!(cache.drain(1).unwrap(), 1);
    assert_eq!(cache.peek_batch_checked(50).unwrap(), vec![sample(3000)]);
    cache.append_sample(&sample(5000)).unwrap();
    assert_eq!(
        cache.peek_batch_checked(50).unwrap(),
        vec![sample(3000), sample(5000)]
    );
    assert_eq!(cache.drain(2).unwrap(), 2);
    assert_eq!(cache.count(), 0);
}

#[test]
fn corrupt_only_cache_can_be_reused_without_losing_the_next_sample() {
    let f = TempCache::new();
    fs::write(&f.0, "bad record\n{incomplete").unwrap();
    let cache = f.cache();
    assert_eq!(cache.count(), 0);
    cache.append_sample(&sample(1000)).unwrap();
    assert_eq!(cache.peek_batch_checked(50).unwrap(), vec![sample(1000)]);
    cache.acknowledge_batch(&[sample(1000)]).unwrap();
    assert_eq!(cache.count(), 0);
}

#[test]
fn acknowledgement_after_capacity_eviction_only_removes_matching_samples() {
    let f = TempCache::new();
    let cache = MetricsCache::with_max_samples(f.0.to_str().unwrap(), 3);
    let pending = vec![sample(1000), sample(3000)];
    cache.append_samples(&pending).unwrap();
    cache.append_samples(&[sample(5000), sample(7000)]).unwrap();
    assert_eq!(cache.acknowledge_batch(&pending).unwrap(), 1);
    assert_eq!(cache.peek_batch(50), vec![sample(5000), sample(7000)]);
    assert_eq!(cache.acknowledge_batch(&pending).unwrap(), 0);
    assert_eq!(cache.count(), 2);
}

#[test]
fn missing_cache_file_is_reported_instead_of_being_treated_as_an_empty_batch() {
    let f = TempCache::new();
    let cache = f.cache();
    cache.append_sample(&sample(1000)).unwrap();
    fs::remove_file(&f.0).unwrap();
    assert!(cache.peek_batch_checked(50).is_err());
}

#[test]
fn corruption_after_startup_does_not_leave_a_permanently_nonempty_queue() {
    let f = TempCache::new();
    let cache = f.cache();
    cache.append_sample(&sample(1000)).unwrap();
    fs::write(&f.0, "invalid partial record").unwrap();
    assert!(cache.peek_batch_checked(50).unwrap().is_empty());
    assert_eq!(cache.count(), 0);
    cache.append_sample(&sample(3000)).unwrap();
    assert_eq!(cache.peek_batch(50), vec![sample(3000)]);
}

#[test]
fn invalid_utf8_record_does_not_hide_valid_records_after_it() {
    let f = TempCache::new();
    let mut bytes = vec![0xff, 0xfe, b'\n'];
    bytes.extend(serde_json::to_vec(&sample(1000)).unwrap());
    bytes.push(b'\n');
    fs::write(&f.0, bytes).unwrap();
    let cache = f.cache();
    assert_eq!(cache.count(), 1);
    assert_eq!(cache.peek_batch_checked(50).unwrap(), vec![sample(1000)]);
    assert_eq!(cache.acknowledge_batch(&[sample(1000)]).unwrap(), 1);
    assert_eq!(cache.count(), 0);
}
