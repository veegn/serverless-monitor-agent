use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WARN_REPEAT_INTERVAL: Duration = Duration::from_secs(10 * 60);
pub const DEFAULT_MAX_LOG_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

#[derive(Debug, Clone)]
pub struct Logger {
    pub debug: bool,
    pub timestamp: bool,
    seen: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Logger {
    pub fn new(debug: bool) -> Self {
        Self {
            debug,
            timestamp: !Self::logging_under_journald(),
            seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn logging_under_journald() -> bool {
        std::env::var_os("JOURNAL_STREAM").is_some() || std::env::var_os("INVOCATION_ID").is_some()
    }

    pub fn info(&self, msg: &str) {
        self.emit("INFO", msg);
    }

    pub fn debug(&self, msg: &str) {
        if self.debug {
            self.emit("DEBUG", msg);
        }
    }

    pub fn warn(&self, msg: &str) {
        self.emit("WARN", msg);
    }

    pub fn warn_once(&self, key: &str, msg: &str) {
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        if let Some(&last) = seen.get(key) {
            if now.duration_since(last) < WARN_REPEAT_INTERVAL {
                return;
            }
        }
        seen.insert(key.to_string(), now);
        drop(seen);
        self.emit("WARN", msg);
    }

    pub fn error(&self, msg: &str) {
        self.emit("ERROR", msg);
    }

    pub fn error_once(&self, key: &str, msg: &str) {
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        if let Some(&last) = seen.get(key) {
            if now.duration_since(last) < WARN_REPEAT_INTERVAL {
                return;
            }
        }
        seen.insert(key.to_string(), now);
        drop(seen);
        self.emit("ERROR", msg);
    }

    fn emit(&self, level: &str, message: &str) {
        if self.timestamp {
            // ISO 8601 / RFC 3339 UTC timestamp
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            if level == "ERROR" {
                eprintln!("[{}] {} {}", level, now, message);
            } else {
                println!("[{}] {} {}", level, now, message);
            }
        } else if level == "ERROR" {
            eprintln!("[{}] {}", level, message);
        } else {
            println!("[{}] {}", level, message);
        }
    }
}

/// Lightweight copy-truncate log rotation to prevent unbounded disk growth in non-systemd environments.
pub fn maybe_rotate_log_file(path: &str, max_bytes: u64) -> bool {
    if path.trim().is_empty() {
        return false;
    }
    let p = Path::new(path);
    if let Ok(meta) = fs::metadata(p) {
        if meta.len() >= max_bytes {
            let backup_path = format!("{}.1", path);
            let _ = fs::copy(p, &backup_path);
            let _ = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(p);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_logger_dedup() {
        let logger = Logger::new(false);
        logger.warn_once("key1", "test warning 1");
        // Second call with same key should be suppressed by dedup window
        logger.warn_once("key1", "test warning 1 again");
        logger.error_once("err1", "test error 1");
        logger.error_once("err1", "test error 1 again");
        assert!(logger.seen.lock().unwrap().contains_key("key1"));
        assert!(logger.seen.lock().unwrap().contains_key("err1"));
    }

    #[test]
    fn test_log_file_rotation() {
        let tmp_file = std::env::temp_dir().join("sma_log_rotation_test.log");
        let path = tmp_file.to_string_lossy().to_string();
        let backup_path = format!("{}.1", path);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&backup_path);

        // Write 100 bytes
        {
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(&[b'a'; 100]).unwrap();
        }

        // Test with max_bytes = 200 (should not rotate)
        assert!(!maybe_rotate_log_file(&path, 200));
        assert!(!Path::new(&backup_path).exists());

        // Test with max_bytes = 50 (should rotate)
        assert!(maybe_rotate_log_file(&path, 50));
        assert!(Path::new(&backup_path).exists());
        assert_eq!(fs::metadata(&backup_path).unwrap().len(), 100);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&backup_path);
    }
}
