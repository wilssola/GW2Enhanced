//! Settings that can change while the game is running.
//!
//! `config.ini` is read once at startup; the overlay then toggles these
//! flags, which the hooks read on every call, and persists the change with
//! `Config::save_key`. Changing the language still needs a game restart
//! (reloading ~200k strings mid-game is not worth it).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use crate::config::{Config, Contribute, Device};
use crate::log;

static TRANSLATE: AtomicBool = AtomicBool::new(true);
static LIVE_TRANSLATE: AtomicBool = AtomicBool::new(true);
static AUTOCOLLECT: AtomicBool = AtomicBool::new(true);
static SEED_DECODE: AtomicBool = AtomicBool::new(true);
static CONTRIBUTE: AtomicU8 = AtomicU8::new(0);
static DEVICE: AtomicU8 = AtomicU8::new(0);
static THREADS: AtomicU32 = AtomicU32::new(0);
static BUBBLE_WAIT: AtomicU32 = AtomicU32::new(8);
static ONBOARDED: AtomicBool = AtomicBool::new(false);
/// The translator must be restarted (device or threads changed).
static RESTART_TRANSLATOR: AtomicBool = AtomicBool::new(false);
static CONFIG_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// A setting the overlay can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Translate,
    LiveTranslate,
    Autocollect,
    SeedDecode,
}

impl Flag {
    fn atomic(self) -> &'static AtomicBool {
        match self {
            Flag::Translate => &TRANSLATE,
            Flag::LiveTranslate => &LIVE_TRANSLATE,
            Flag::Autocollect => &AUTOCOLLECT,
            Flag::SeedDecode => &SEED_DECODE,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Flag::Translate => "translate",
            Flag::LiveTranslate => "live_translate",
            Flag::Autocollect => "autocollect",
            Flag::SeedDecode => "seed_decode",
        }
    }
}

fn contribute_code(c: Contribute) -> u8 {
    match c {
        Contribute::Ask => 0,
        Contribute::Yes => 1,
        Contribute::No => 2,
    }
}

pub fn init(config: &Config, config_path: std::path::PathBuf) {
    TRANSLATE.store(config.translate, Ordering::Relaxed);
    LIVE_TRANSLATE.store(config.live_translate, Ordering::Relaxed);
    AUTOCOLLECT.store(config.autocollect, Ordering::Relaxed);
    SEED_DECODE.store(config.seed_decode, Ordering::Relaxed);
    CONTRIBUTE.store(contribute_code(config.contribute), Ordering::Relaxed);
    DEVICE.store(device_code(config.translator_device), Ordering::Relaxed);
    THREADS.store(config.translator_threads, Ordering::Relaxed);
    BUBBLE_WAIT.store(config.bubble_wait, Ordering::Relaxed);
    ONBOARDED.store(config.onboarded, Ordering::Relaxed);
    let _ = CONFIG_PATH.set(config_path);
}

pub fn get(flag: Flag) -> bool {
    flag.atomic().load(Ordering::Relaxed)
}

/// Changes a setting now and saves it to `config.ini`.
pub fn set(flag: Flag, on: bool) {
    flag.atomic().store(on, Ordering::Relaxed);
    save(flag.key(), if on { "true" } else { "false" });
}

pub fn contribute() -> Contribute {
    match CONTRIBUTE.load(Ordering::Relaxed) {
        1 => Contribute::Yes,
        2 => Contribute::No,
        _ => Contribute::Ask,
    }
}

pub fn set_contribute(on: bool) {
    CONTRIBUTE.store(
        contribute_code(if on { Contribute::Yes } else { Contribute::No }),
        Ordering::Relaxed,
    );
    save("contribute", if on { "true" } else { "false" });
}

fn device_code(d: Device) -> u8 {
    Device::ALL.iter().position(|&x| x == d).unwrap_or(0) as u8
}

/// Where the local translator should run.
pub fn device() -> Device {
    Device::ALL
        .get(DEVICE.load(Ordering::Relaxed) as usize)
        .copied()
        .unwrap_or(Device::Auto)
}

/// CPU threads for the translator; 0 = automatic.
pub fn threads() -> u32 {
    THREADS.load(Ordering::Relaxed)
}

/// Changes where the translator runs; it restarts with the new setting.
pub fn set_device(d: Device) {
    DEVICE.store(device_code(d), Ordering::Relaxed);
    RESTART_TRANSLATOR.store(true, Ordering::Relaxed);
    save("translator_device", d.key());
}

pub fn set_threads(n: u32) {
    THREADS.store(n, Ordering::Relaxed);
    RESTART_TRANSLATOR.store(true, Ordering::Relaxed);
    save("translator_threads", &n.to_string());
}

/// Taken by the translator thread: restart with the current settings.
pub fn take_restart() -> bool {
    RESTART_TRANSLATOR.swap(false, Ordering::Relaxed)
}

/// Longest extra time for a speech bubble waiting for its translation.
pub fn bubble_wait() -> u32 {
    BUBBLE_WAIT.load(Ordering::Relaxed)
}

pub fn set_bubble_wait(seconds: u32) {
    BUBBLE_WAIT.store(seconds, Ordering::Relaxed);
    save("bubble_wait", &seconds.to_string());
}

pub fn onboarded() -> bool {
    ONBOARDED.load(Ordering::Relaxed)
}

pub fn set_onboarded() {
    ONBOARDED.store(true, Ordering::Relaxed);
    save("onboarded", "true");
}

/// Saves a key that only takes effect after a restart (the language).
pub fn save(key: &str, value: &str) {
    let Some(path) = CONFIG_PATH.get() else { return };
    if let Err(e) = Config::save_key(path, key, value) {
        log!("config: could not save {key}={value}: {e}");
    }
}
