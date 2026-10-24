use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

pub const MAX_CACHED_SAMPLES: usize = 5000;
pub const BATCH_REPORT_SIZE: usize = 50;
const CACHE_COMPACT_MIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedSample {
    pub ts: i64,
    pub metrics: serde_json::Value,
}

#[derive(Debug, Default)]
struct CacheState {
    count: usize,
    offset: u64,
}

#[derive(Debug)]
pub struct MetricsCache {
    path: String,
    cursor_path: String,
    max_samples: usize,
    state: Mutex<CacheState>,
}

impl MetricsCache {
    pub fn new(path: &str) -> Self {
        Self::with_max_samples(path, MAX_CACHED_SAMPLES)
    }

    pub fn with_max_samples(path: &str, max_samples: usize) -> Self {
        let cursor_path = format!("{}.cursor", path);
        let state = load_state(path, &cursor_path);
        let cache = Self {
            path: path.to_string(),
            cursor_path,
            max_samples,
            state: Mutex::new(state),
        };
        if let Ok(mut state) = cache.state.lock() {
            let _ = cache.enforce_capacity(&mut state);
        }
        cache
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn count(&self) -> usize {
        self.state.lock().unwrap().count
    }

    pub fn append_sample(&self, sample: &CachedSample) -> Result<(), String> {
        self.append_samples(std::slice::from_ref(sample))
            .map(|_| ())
    }

    pub fn append_samples(&self, samples: &[CachedSample]) -> Result<usize, String> {
        if samples.is_empty() {
            return Ok(0);
        }
        if let Some(parent) = Path::new(&self.path).parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let mut encoded = Vec::with_capacity(samples.len());
        for sample in samples {
            encoded.push(serde_json::to_vec(sample).map_err(|e| e.to_string())?);
        }

        let mut state = self.state.lock().unwrap();
        if state.count == 0 && state.offset == 0 {
            // A previous removal may have left a stale cursor sidecar behind.
            // Reset it before starting a new queue file.
            self.persist_cursor(0)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("failed to open cache file {}: {}", self.path, e))?;

        for line in &encoded {
            file.write_all(line).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        file.flush().map_err(|e| e.to_string())?;

        state.count = state.count.saturating_add(encoded.len());
        self.enforce_capacity(&mut state)?;
        Ok(encoded.len())
    }

    pub fn peek_batch(&self, limit: usize) -> Vec<CachedSample> {
        if limit == 0 {
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(state.offset)).is_err() {
            return Vec::new();
        }
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
        let mut state = self.state.lock().unwrap();
        if state.count == 0 {
            return Ok(0);
        }
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Ok(0),
        };
        let mut reader = BufReader::new(file);
        reader
            .seek(SeekFrom::Start(state.offset))
            .map_err(|e| e.to_string())?;

        let mut skipped = 0usize;
        let mut line = Vec::new();
        while skipped < count.min(state.count) {
            line.clear();
            let bytes = reader
                .read_until(b'\n', &mut line)
                .map_err(|e| e.to_string())?;
            if bytes == 0 {
                break;
            }
            if !line.iter().all(|b| b.is_ascii_whitespace()) {
                skipped += 1;
            }
        }

        state.offset = reader.stream_position().map_err(|e| e.to_string())?;
        state.count = state.count.saturating_sub(skipped);
        drop(reader);
        if state.count == 0 {
            let file_len = fs::metadata(&self.path)
                .map(|metadata| metadata.len())
                .unwrap_or(state.offset);
            match fs::remove_file(&self.path) {
                Ok(()) => {
                    let _ = fs::remove_file(&self.cursor_path);
                    state.offset = 0;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let _ = fs::remove_file(&self.cursor_path);
                    state.offset = 0;
                }
                Err(_) => {
                    state.offset = file_len;
                    self.persist_cursor(state.offset)?;
                }
            }
        } else {
            self.persist_cursor(state.offset)?;
            self.compact_if_needed(&mut state)?;
        }
        Ok(skipped)
    }

    pub fn clear(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if Path::new(&self.path).exists() {
            fs::remove_file(&self.path).map_err(|e| e.to_string())?;
        }
        *state = CacheState::default();
        if Path::new(&self.cursor_path).exists() {
            fs::remove_file(&self.cursor_path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn enforce_capacity(&self, state: &mut CacheState) -> Result<(), String> {
        if state.count > self.max_samples {
            let excess = state.count - self.max_samples;
            let file = File::open(&self.path).map_err(|e| e.to_string())?;
            let mut reader = BufReader::new(file);
            reader
                .seek(SeekFrom::Start(state.offset))
                .map_err(|e| e.to_string())?;
            let mut skipped = 0usize;
            let mut line = Vec::new();
            while skipped < excess {
                line.clear();
                let bytes = reader
                    .read_until(b'\n', &mut line)
                    .map_err(|e| e.to_string())?;
                if bytes == 0 {
                    break;
                }
                if !line.iter().all(|b| b.is_ascii_whitespace()) {
                    skipped += 1;
                }
            }
            state.offset = reader.stream_position().map_err(|e| e.to_string())?;
            state.count = state.count.saturating_sub(skipped);
            self.persist_cursor(state.offset)?;
        }
        self.compact_if_needed(state)
    }

    fn persist_cursor(&self, offset: u64) -> Result<(), String> {
        let mut cursor = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.cursor_path)
            .map_err(|e| e.to_string())?;
        writeln!(cursor, "{}", offset).map_err(|e| e.to_string())?;
        cursor.sync_all().map_err(|e| e.to_string())
    }

    fn compact_if_needed(&self, state: &mut CacheState) -> Result<(), String> {
        if state.offset == 0 {
            return Ok(());
        }
        let file_len = match fs::metadata(&self.path) {
            Ok(meta) => meta.len(),
            Err(_) => return Ok(()),
        };
        if state.offset < CACHE_COMPACT_MIN_BYTES && state.offset.saturating_mul(2) < file_len {
            return Ok(());
        }

        let tmp_path = format!("{}.compact", self.path);
        let mut source = File::open(&self.path).map_err(|e| e.to_string())?;
        source
            .seek(SeekFrom::Start(state.offset))
            .map_err(|e| e.to_string())?;
        let mut target = File::create(&tmp_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
        drop(source);
        target.flush().map_err(|e| e.to_string())?;
        target.sync_all().map_err(|e| e.to_string())?;
        drop(target);
        // Persist the zero cursor before replacing the compacted data file.
        // If interrupted before rename, the old records may be replayed, but
        // no queued samples can be skipped after a restart.
        let previous_offset = state.offset;
        self.persist_cursor(0)?;
        if let Err(error) = replace_file(Path::new(&tmp_path), Path::new(&self.path)) {
            let _ = fs::remove_file(&tmp_path);
            self.persist_cursor(previous_offset)?;
            return Err(error.to_string());
        }
        state.offset = 0;
        Ok(())
    }
}

fn load_state(path: &str, cursor_path: &str) -> CacheState {
    let file_len = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if file_len == 0 {
        return CacheState::default();
    }

    let mut offset = fs::read_to_string(cursor_path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if offset > file_len {
        offset = 0;
    }
    let count = count_records_from(path, offset);
    CacheState { count, offset }
}

fn count_records_from(path: &str, offset: u64) -> usize {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return 0,
    };
    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(offset)).is_err() {
        return 0;
    }
    reader
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .count()
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
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
