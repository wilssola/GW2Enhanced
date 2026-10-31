//! Log in `gw2remastered_log.txt`, in the format `[seconds] message`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Opens (truncating) the log. Until then, messages are discarded. The previous session's log is
/// kept as `<name>.prev.txt`, so the log of a session that crashed survives the next launch.
pub fn init(path: &Path) {
    START.get_or_init(Instant::now);
    let _ = std::fs::rename(path, path.with_extension("prev.txt"));
    let f = OpenOptions::new().create(true).write(true).truncate(true).open(path).ok();
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = f;
}

pub fn write(msg: &str) {
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    if let Some(f) = FILE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = writeln!(f, "[{t:8.3}] {msg}");
        let _ = f.flush();
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
