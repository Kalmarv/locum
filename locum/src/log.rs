//! locum's own log: `locum.log` in the folder named by `LOCUM_DIR`, or `%LOCALAPPDATA%\locum` without it. Never the
//! game's folder.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Log {
    start: Instant,
    file: Mutex<Option<File>>,
}

fn log_state() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    LOG.get_or_init(|| {
        let file = folder().and_then(|dir| {
            std::fs::create_dir_all(&dir).ok()?;
            File::create(dir.join("locum.log")).ok()
        });
        Log {
            start: Instant::now(),
            file: Mutex::new(file),
        }
    })
}

/// The folder locum writes to.
pub fn folder() -> Option<PathBuf> {
    match std::env::var_os("LOCUM_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("locum")),
    }
}

/// Appends one line, prefixed with the seconds since the first line.
pub fn line(text: &str) {
    let log = log_state();
    let secs = log.start.elapsed().as_secs_f64();
    if let Ok(mut file) = log.file.lock()
        && let Some(file) = file.as_mut()
    {
        let _ = writeln!(file, "[{secs:10.3}] {text}");
        let _ = file.flush();
    }
}

/// `log!("format", args)`: a formatted [`line`].
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}
pub(crate) use log;
