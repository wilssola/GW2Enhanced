//! Log in `gw2remastered_log.txt`, in the format `[seconds] message`.
//!
//! Off by default: written only with `log_textures=1` in `GW2Enhanced\config.ini` (or the Logs switch in the
//! overlay's Health tab), see `gw2enhanced_core::logs`. The previous session's log is kept as `<name>.prev.txt`, so
//! the log of a session that crashed survives the next launch.

use std::path::Path;

use gw2enhanced_core::logs::TEXTURES;

/// Where the log goes (opened only while logging is on). Until then, messages are discarded.
pub fn init(path: &Path) {
    TEXTURES.init(path);
}

pub fn write(msg: &str) {
    TEXTURES.write(msg);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
