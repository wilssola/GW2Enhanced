//! Log in `gw2graphics_log.txt`, in the format `[seconds] message`.
//!
//! Off by default: written only with `log_graphics=1` in `GW2Enhanced\config.ini` (or the Logs switch in the
//! overlay's Health tab), see `gw2enhanced_core::logs`. The previous session's log is kept as `<name>.prev.txt`, so
//! the log of a session that crashed survives the next launch.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use gw2enhanced_core::logs::GRAPHICS;

static START: OnceLock<Instant> = OnceLock::new();

/// Where the log goes (opened only while logging is on). Until then, messages are discarded.
pub fn init(path: &Path) {
    START.get_or_init(Instant::now);
    GRAPHICS.init(path);
}

/// Milliseconds since the start (monotonic).
pub fn uptime_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn write(msg: &str) {
    START.get_or_init(Instant::now);
    GRAPHICS.write(msg);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
