//! Minimal logger that mirrors the desktop Worker's behavior: records are
//! appended to `worker.log` beside the configuration, and also written to
//! stderr, which iOS captures in the unified log for attached sessions.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};

/// Keep worker.log bounded; it is a debugging aid, not an audit trail.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

struct WorkerLogger {
    file: Mutex<std::fs::File>,
}

impl Log for WorkerLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &Record) {
        let line = format!("{} [{}] {}\n", timestamp(), record.level(), record.args());
        eprint!("{line}");
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {}
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

pub fn install(config_dir: &Path) {
    let path = config_dir.join("worker.log");
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
        let _ = std::fs::remove_file(&path);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = log::set_boxed_logger(Box::new(WorkerLogger {
        file: Mutex::new(file),
    }));
    log::set_max_level(LevelFilter::Info);
}
