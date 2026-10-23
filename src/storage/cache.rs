use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

pub const MAX_CACHED_SAMPLES: usize = 5000;
pub const BATCH_REPORT_SIZE: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedSample {
    pub ts: i64,
    pub metrics: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct MetricsCache {
    path: String,
    max_samples: usize,
}

impl MetricsCache {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            max_samples: MAX_CACHED_SAMPLES,
        }
    }

    pub fn with_max_samples(path: &str, max_samples: usize) -> Self {
        Self {
            path: path.to_string(),
            max_samples,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn count(&self) -> usize {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return 0,
        };
        let reader = BufReader::new(file);
        reader
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    pub fn append_sample(&self, sample: &CachedSample) -> Result<(), String> {
        self.append_samples(std::slice::from_ref(sample)).map(|_| ())
    }

    pub fn append_samples(&self, samples: &[CachedSample]) -> Result<usize, String> {
        if samples.is_empty() {
            return Ok(0);
        }
        if let Some(parent) = Path::new(&self.path).parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("failed to open cache file {}: {}", self.path, e))?;

        let mut written = 0;
        for s in samples {
            if let Ok(line) = serde_json::to_string(s) {
                if writeln!(file, "{}", line).is_ok() {
                    written += 1;
                }
            }
        }
        let _ = file.flush();

        self.enforce_capacity();
        Ok(written)
    }

    pub fn peek_batch(&self, limit: usize) -> Vec<CachedSample> {
        if limit == 0 {
            return Vec::new();
        }
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let reader = BufReader::new(file);
        let mut batch = Vec::with_capacity(limit);

        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(sample) = serde_json::from_str::<CachedSample>(trimmed) {
                batch.push(sample);
                if batch.len() >= limit {
                    break;
                }
            }
        }
        batch
    }

    pub fn drain(&self, count: usize) -> Result<usize, String> {
        if count == 0 {
            return Ok(0);
        }
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Ok(0),
        };
        let reader = BufReader::new(file);
        let mut remaining = Vec::new();
        let mut skipped = 0;

        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if skipped < count {
                skipped += 1;
            } else {
                remaining.push(trimmed.to_string());
            }
        }

        if remaining.is_empty() {
            let _ = fs::remove_file(&self.path);
        } else {
            let tmp_path = format!("{}.tmp", self.path);
            let mut tmp_file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| format!("failed to write tmp cache {}: {}", tmp_path, e))?;

            for l in remaining {
                let _ = writeln!(tmp_file, "{}", l);
            }
            let _ = tmp_file.flush();
            let _ = fs::rename(&tmp_path, &self.path);
        }

        Ok(skipped)
    }

    pub fn clear(&self) -> Result<(), String> {
        if Path::new(&self.path).exists() {
            fs::remove_file(&self.path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn enforce_capacity(&self) {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .collect();

        if lines.len() > self.max_samples {
            let excess = lines.len() - self.max_samples;
            let to_keep = &lines[excess..];
            let tmp_path = format!("{}.tmp", self.path);
            if let Ok(mut tmp_file) = File::create(&tmp_path) {
                for l in to_keep {
                    let _ = writeln!(tmp_file, "{}", l);
                }
                let _ = tmp_file.flush();
                let _ = fs::rename(&tmp_path, &self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_append_peek_drain() {
        let tmp_dir = std::env::temp_dir().join("sma_cache_test_1");
        let _ = fs::remove_dir_all(&tmp_dir);
        let cache_file = tmp_dir.join("metrics_cache.dat");
        let cache = MetricsCache::new(&cache_file.to_string_lossy());

        assert_eq!(cache.count(), 0);
        assert!(cache.peek_batch(10).is_empty());

        let samples = vec![
            CachedSample {
                ts: 1000,
                metrics: serde_json::json!({"cpu": "1.0"}),
            },
            CachedSample {
                ts: 2000,
                metrics: serde_json::json!({"cpu": "2.0"}),
            },
            CachedSample {
                ts: 3000,
                metrics: serde_json::json!({"cpu": "3.0"}),
            },
        ];

        let written = cache.append_samples(&samples).unwrap();
        assert_eq!(written, 3);
        assert_eq!(cache.count(), 3);

        let batch = cache.peek_batch(2);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].ts, 1000);
        assert_eq!(batch[1].ts, 2000);

        // Still 3 in cache
        assert_eq!(cache.count(), 3);

        // Drain first 2
        let drained = cache.drain(2).unwrap();
        assert_eq!(drained, 2);
        assert_eq!(cache.count(), 1);

        let rem = cache.peek_batch(10);
        assert_eq!(rem.len(), 1);
        assert_eq!(rem[0].ts, 3000);

        // Drain remaining
        let drained_rem = cache.drain(1).unwrap();
        assert_eq!(drained_rem, 1);
        assert_eq!(cache.count(), 0);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_cache_capacity_enforcement() {
        let tmp_dir = std::env::temp_dir().join("sma_cache_test_2");
        let _ = fs::remove_dir_all(&tmp_dir);
        let cache_file = tmp_dir.join("metrics_cache.dat");
        let cache = MetricsCache::with_max_samples(&cache_file.to_string_lossy(), 3);

        let samples: Vec<CachedSample> = (1..=5)
            .map(|i| CachedSample {
                ts: i * 1000,
                metrics: serde_json::json!({"val": i}),
            })
            .collect();

        cache.append_samples(&samples).unwrap();
        assert_eq!(cache.count(), 3);

        let batch = cache.peek_batch(10);
        assert_eq!(batch.len(), 3);
        // Oldest 1 and 2 should be dropped, keeping 3, 4, 5
        assert_eq!(batch[0].ts, 3000);
        assert_eq!(batch[1].ts, 4000);
        assert_eq!(batch[2].ts, 5000);

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
