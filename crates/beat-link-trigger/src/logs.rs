//! Logging: to standard error, to a rotating file in the logs folder, and to an
//! in-memory buffer shown in the Logs window. Port of `logs.clj`.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// One captured log line.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: String,
    pub level: log::Level,
    pub target: String,
    pub message: String,
}

/// Recent log lines, shared with the user interface.
#[derive(Default)]
pub struct LogBuffer {
    lines: Mutex<VecDeque<LogLine>>,
}

const MAX_LINES: usize = 2000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

impl LogBuffer {
    pub fn lines(&self) -> Vec<LogLine> {
        self.lines.lock().iter().cloned().collect()
    }
    pub fn clear(&self) {
        self.lines.lock().clear();
    }
    fn push(&self, line: LogLine) {
        let mut l = self.lines.lock();
        if l.len() >= MAX_LINES {
            l.pop_front();
        }
        l.push_back(line);
    }
}

struct Logger {
    level: log::LevelFilter,
    buffer: Arc<LogBuffer>,
    file: Mutex<Option<(PathBuf, File)>>,
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Keep third-party chatter (HTTP, GUI internals) out unless at warn or above.
        let target = record.target();
        let ours = target.starts_with("beat_link") || target == "expression";
        if !ours && record.level() > log::Level::Warn {
            return;
        }
        let line = LogLine {
            time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            level: record.level(),
            target: target.to_string(),
            message: record.args().to_string(),
        };
        let text = format!("{} {:5} [{}] {}\n", line.time, line.level, line.target, line.message);
        let _ = std::io::stderr().write_all(text.as_bytes());
        if let Some((path, file)) = self.file.lock().as_mut() {
            let _ = file.write_all(text.as_bytes());
            if file.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(false) {
                let rotated = path.with_extension("log.1");
                let _ = fs::rename(&*path, rotated);
                if let Ok(f) = OpenOptions::new().create(true).append(true).open(&*path) {
                    *file = f;
                }
            }
        }
        self.buffer.push(line);
    }

    fn flush(&self) {
        if let Some((_, f)) = self.file.lock().as_mut() {
            let _ = f.flush();
        }
    }
}

static BUFFER: OnceLock<Arc<LogBuffer>> = OnceLock::new();

/// Install the logger (once). Returns the shared buffer.
pub fn init(level: log::LevelFilter, file: Option<PathBuf>) -> Arc<LogBuffer> {
    let buffer = BUFFER.get_or_init(|| Arc::new(LogBuffer::default())).clone();
    let opened = file.and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        OpenOptions::new().create(true).append(true).open(&p).ok().map(|f| (p, f))
    });
    let logger = Logger { level, buffer: buffer.clone(), file: Mutex::new(opened) };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    buffer
}

/// The shared buffer (creating an empty one if logging was never initialized).
pub fn buffer() -> Arc<LogBuffer> {
    BUFFER.get_or_init(|| Arc::new(LogBuffer::default())).clone()
}

/// Where log files are kept.
pub fn log_file_path() -> Option<PathBuf> {
    crate::config::app_dirs().map(|d| d.data_local_dir().join("logs").join("blt.log"))
}
