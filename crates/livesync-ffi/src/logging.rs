//! Process-wide log sink for the app: every formatted line goes to an
//! in-memory ring (the debug screen's "last lines"), to a size-capped log
//! file in the app's private storage (what "export logs" collects), and on
//! Android to logcat (what an adb session reads).
//!
//! Logging is the one process-wide piece of the FFI; engines themselves stay
//! per profile. Nothing here sees credentials: the engine never logs them.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

/// Lines kept in memory.
pub const RING_LINES: usize = 1000;
/// The log file is rotated (to `.1`) once it passes this size.
pub const ROTATE_BYTES: u64 = 4 * 1024 * 1024;
pub const LOG_FILE: &str = "livesync.log";

struct Sink {
    ring: VecDeque<String>,
    file: Option<File>,
    path: Option<PathBuf>,
    written: u64,
}

impl Sink {
    fn push(&mut self, line: &str) {
        let line = line.trim_end_matches('\n');
        if line.is_empty() {
            return;
        }
        if self.ring.len() == RING_LINES {
            self.ring.pop_front();
        }
        self.ring.push_back(line.to_string());
        if let Some(f) = self.file.as_mut() {
            if writeln!(f, "{line}").is_ok() {
                self.written += line.len() as u64 + 1;
            }
        }
        if self.written > ROTATE_BYTES {
            self.rotate();
        }
    }

    fn rotate(&mut self) {
        let Some(path) = self.path.clone() else { return };
        self.file = None;
        let _ = std::fs::rename(&path, rotated(&path));
        self.file = open_append(&path).ok();
        self.written = 0;
    }
}

fn rotated(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn sink() -> &'static Mutex<Sink> {
    static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
    SINK.get_or_init(|| {
        Mutex::new(Sink {
            ring: VecDeque::with_capacity(RING_LINES),
            file: None,
            path: None,
            written: 0,
        })
    })
}

/// Point the log file at `dir/livesync.log` (created if needed). Later calls
/// with another folder switch files.
pub fn set_log_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    let file = open_append(&path)?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut s = sink().lock().unwrap();
    s.file = Some(file);
    s.path = Some(path);
    s.written = written;
    if s.written > ROTATE_BYTES {
        s.rotate();
    }
    Ok(())
}

/// The newest `max` lines, oldest first.
pub fn recent_lines(max: usize) -> Vec<String> {
    let s = sink().lock().unwrap();
    let skip = s.ring.len().saturating_sub(max);
    s.ring.iter().skip(skip).cloned().collect()
}

/// The log files on disk, oldest first (`livesync.log.1`, then `livesync.log`).
pub fn log_files() -> Vec<String> {
    let s = sink().lock().unwrap();
    let Some(path) = s.path.clone() else { return vec![] };
    [rotated(&path), path]
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect()
}

/// Install the global subscriber once: INFO for our crates and the app,
/// WARN for libraries (hyper, reqwest and rustls are chatty at INFO/DEBUG).
pub fn init() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let filter = tracing_subscriber::EnvFilter::new(
            "warn,livesync_engine=info,livesync_remote=info,livesync_model=info,livesync_ffi=info,app=info",
        );
        let sub = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_target(false)
            .with_writer(TeeWriter)
            .finish();
        if tracing::subscriber::set_global_default(sub).is_ok() {
            // Our own `log` bridge: it passes known-harmless library warnings on at debug.
            let _ = livesync_engine::logbridge::init();
        }
    });
}

/// `MakeWriter` handing out one buffer per event; the line is delivered when
/// the buffer is dropped, so a line is never split between sinks.
#[derive(Clone, Copy)]
struct TeeWriter;

struct LineBuf {
    buf: Vec<u8>,
    level: tracing::Level,
}

impl<'a> MakeWriter<'a> for TeeWriter {
    type Writer = LineBuf;
    fn make_writer(&'a self) -> LineBuf {
        LineBuf {
            buf: Vec::with_capacity(256),
            level: tracing::Level::INFO,
        }
    }
    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> LineBuf {
        LineBuf {
            buf: Vec::with_capacity(256),
            level: *meta.level(),
        }
    }
}

impl Write for LineBuf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineBuf {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        logcat::write(self.level, &text);
        if let Ok(mut s) = sink().lock() {
            for line in text.lines() {
                s.push(line);
            }
        }
    }
}

#[cfg(target_os = "android")]
mod logcat {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn write(level: tracing::Level, text: &str) {
        // android/log.h priorities.
        let prio = match level {
            tracing::Level::ERROR => 6,
            tracing::Level::WARN => 5,
            tracing::Level::INFO => 4,
            tracing::Level::DEBUG => 3,
            tracing::Level::TRACE => 2,
        };
        let tag = c"livesync";
        if let Ok(msg) = CString::new(text.trim_end().replace('\0', " ")) {
            unsafe {
                __android_log_write(prio, tag.as_ptr(), msg.as_ptr());
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
mod logcat {
    pub fn write(_level: tracing::Level, _text: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_lines_and_the_file_rotates() {
        let dir = tempfile::tempdir().unwrap();
        set_log_dir(dir.path()).unwrap();
        {
            let mut s = sink().lock().unwrap();
            for i in 0..(RING_LINES + 5) {
                s.push(&format!("line {i}\n"));
            }
        }
        let last = recent_lines(3);
        assert_eq!(last.len(), 3);
        assert_eq!(last[2], format!("line {}", RING_LINES + 4));
        assert!(recent_lines(usize::MAX).len() <= RING_LINES);
        let files = log_files();
        assert_eq!(files.len(), 1, "no rotation yet: {files:?}");
        {
            let mut s = sink().lock().unwrap();
            s.written = ROTATE_BYTES + 1;
            s.push("after rotation");
        }
        let files = log_files();
        assert_eq!(files.len(), 2, "rotated: {files:?}");
        assert!(files[0].ends_with(".1"));
    }
}
