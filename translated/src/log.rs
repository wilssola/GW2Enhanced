//! Log in `gw2translated_log.txt`, in the format `[seconds] message`.
//!
//! Off by default: written only with `log_translation=1` in `GW2Enhanced\config.ini` (or the Logs switch in the
//! overlay's Health tab), see `gw2enhanced_core::logs`.

use std::path::Path;

use gw2enhanced_core::logs::TRANSLATION;

/// Where the log goes (opened only while logging is on). Until then, messages are discarded.
pub fn init(path: &Path) {
    TRANSLATION.init(path);
}

pub fn write(msg: &str) {
    TRANSLATION.write(msg);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
