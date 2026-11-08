//! Persistent diagnostics in %APPDATA%\batchviproc\logs:
//! - `batchviproc.log`: the activity log, rotated to `batchviproc.1.log` at 5 MB;
//! - `crash-<timestamp>.txt`: written by the panic hook before the process aborts
//!   (release builds have no console, so this is the only trace of a crash).

use crate::system;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileLog {
    path: PathBuf,
    rotated: PathBuf,
    state: Mutex<Option<(File, u64)>>,
}

impl FileLog {
    pub fn open(dir: &Path) -> Self {
        let _ = fs::create_dir_all(dir);
        let path = dir.join("batchviproc.log");
        let rotated = dir.join("batchviproc.1.log");
        let state = Self::open_file(&path);
        Self {
            path,
            rotated,
            state: Mutex::new(state),
        }
    }

    fn open_file(path: &Path) -> Option<(File, u64)> {
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        let size = file.metadata().map_or(0, |m| m.len());
        Some((file, size))
    }

    /// Appends one line, prefixed with the local date and time. Never fails loudly:
    /// logging must not break processing.
    pub fn write_line(&self, message: &str) {
        let line = format!("{} {message}\r\n", system::local_datetime());
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.as_ref().is_some_and(|(_, size)| *size + line.len() as u64 > MAX_LOG_BYTES) {
            *state = None; // close before renaming
            let _ = fs::rename(&self.path, &self.rotated);
            *state = Self::open_file(&self.path);
        }
        if let Some((file, size)) = state.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                *size += line.len() as u64;
            }
        }
    }
}

/// Writes a crash report for any panic, then lets the default hook run.
pub fn install_panic_hook(dir: PathBuf) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("crash-{}.txt", system::local_timestamp_compact()));
        let thread = std::thread::current();
        let report = format!(
            "batchviproc {} crashed at {}\r\nthread: {}\r\n{info}\r\n\r\nbacktrace:\r\n{}\r\n",
            env!("CARGO_PKG_VERSION"),
            system::local_datetime(),
            thread.name().unwrap_or("unnamed"),
            std::backtrace::Backtrace::force_capture(),
        );
        let _ = fs::write(&path, report);
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_rotates() {
        let dir = std::env::temp_dir().join(format!("bvp-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = FileLog::open(&dir);
        log.write_line("first line");
        assert!(fs::read_to_string(dir.join("batchviproc.log")).unwrap().contains("first line"));

        // Force a rotation by pretending the file is already full.
        log.state.lock().unwrap().as_mut().unwrap().1 = MAX_LOG_BYTES;
        log.write_line("after rotation");
        assert!(fs::read_to_string(dir.join("batchviproc.1.log")).unwrap().contains("first line"));
        let current = fs::read_to_string(dir.join("batchviproc.log")).unwrap();
        assert!(current.contains("after rotation") && !current.contains("first line"));
        let _ = fs::remove_dir_all(&dir);
    }
}
