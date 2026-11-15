//! Log files of the mod, one per feature, all OFF by default.
//!
//! Each feature writes `[seconds] message` lines to its own file (the path it passes to [`FeatureLog::init`]), but only
//! while its switch is on: `log_host`, `log_translation`, `log_textures`, `log_graphics` (`0|1`) in
//! `GW2Enhanced\config.ini`, also changed in the overlay's Health tab, live. When a log is off nothing is written to
//! disk; the lines that look like problems are still kept in memory (the last [`RECENT`]) so the Health tab can show
//! them.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::settings;

/// Problem lines kept in memory per feature.
pub const RECENT: usize = 30;

/// Who writes the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// GW2 Enhanced itself: the overlay host and the updater.
    Host,
    Translation,
    Textures,
    Graphics,
}

impl Feature {
    pub const ALL: [Feature; 4] = [Feature::Host, Feature::Translation, Feature::Textures, Feature::Graphics];

    /// The switch in `GW2Enhanced\config.ini`.
    pub fn key(self) -> &'static str {
        match self {
            Feature::Host => "log_host",
            Feature::Translation => "log_translation",
            Feature::Textures => "log_textures",
            Feature::Graphics => "log_graphics",
        }
    }

    pub fn log(self) -> &'static FeatureLog {
        match self {
            Feature::Host => &HOST,
            Feature::Translation => &TRANSLATION,
            Feature::Textures => &TEXTURES,
            Feature::Graphics => &GRAPHICS,
        }
    }
}

pub static HOST: FeatureLog = FeatureLog::new(Feature::Host);
pub static TRANSLATION: FeatureLog = FeatureLog::new(Feature::Translation);
pub static TEXTURES: FeatureLog = FeatureLog::new(Feature::Textures);
pub static GRAPHICS: FeatureLog = FeatureLog::new(Feature::Graphics);

/// The switch of `f` in a config text: off unless it says on.
pub fn enabled_in(text: &str, f: Feature) -> bool {
    settings::get(text, f.key()).and_then(|v| settings::parse_bool(&v)).unwrap_or(false)
}

/// A line that is worth keeping for the Health tab when the log is off.
pub fn is_problem(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    ["error", "fail", "could not", "cannot", "can't", "panic", "warn", "refus", "invalid", "not found", "missing"]
        .iter()
        .any(|w| m.contains(w))
}

/// The player turned the log of `f` on or off in the overlay: applied now and saved.
pub fn choose(f: Feature, on: bool) -> std::io::Result<()> {
    f.log().set_enabled(on);
    settings::save(f.key(), if on { "1" } else { "0" }, Some("Log file of this part of the mod (0 = off, the default)"))
}

pub struct FeatureLog {
    feature: Feature,
    enabled: AtomicBool,
    start: OnceLock<Instant>,
    path: Mutex<Option<PathBuf>>,
    file: Mutex<Option<File>>,
    /// The file was opened once this session (it is started fresh the first time, appended to after).
    opened: AtomicBool,
    recent: Mutex<VecDeque<String>>,
}

impl FeatureLog {
    pub const fn new(feature: Feature) -> Self {
        Self {
            feature,
            enabled: AtomicBool::new(false),
            start: OnceLock::new(),
            path: Mutex::new(None),
            file: Mutex::new(None),
            opened: AtomicBool::new(false),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    pub fn feature(&self) -> Feature {
        self.feature
    }

    /// Where the log goes; the switch is read from the shared config. Until this runs nothing is written.
    pub fn init(&self, path: &Path) {
        self.init_with(path, enabled_in(&settings::read(), self.feature));
    }

    /// [`init`](Self::init) with the switch given.
    pub fn init_with(&self, path: &Path, on: bool) {
        self.start.get_or_init(Instant::now);
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
        self.set_enabled(on);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Turns the file on (opened now; the previous session's log is kept as `<name>.prev.txt` the first time) or off
    /// (closed; nothing more is written).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if !on {
            *file = None;
            return;
        }
        if file.is_some() {
            return;
        }
        let Some(path) = self.path.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let first = !self.opened.swap(true, Ordering::Relaxed);
        if first {
            let _ = std::fs::rename(&path, path.with_extension("prev.txt"));
        }
        *file = OpenOptions::new().create(true).write(true).append(!first).truncate(first).open(&path).ok();
    }

    /// The log file, when it is configured.
    pub fn path(&self) -> Option<PathBuf> {
        self.path.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn write(&self, msg: &str) {
        let t = self.start.get_or_init(Instant::now).elapsed().as_secs_f64();
        if is_problem(msg) {
            let mut r = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            if r.len() >= RECENT {
                r.pop_front();
            }
            r.push_back(format!("[{t:8.3}] {msg}"));
        }
        if !self.enabled() {
            return;
        }
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(f, "[{t:8.3}] {msg}");
            let _ = f.flush();
        }
    }

    /// The problem lines kept in memory, oldest first.
    pub fn recent(&self) -> Vec<String> {
        self.recent.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gw2e_logs_test_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_log_is_off_by_default() {
        for f in Feature::ALL {
            assert!(!enabled_in("", f), "{f:?}");
            assert!(!enabled_in("check_updates=false\nui_language=en\n", f), "{f:?}");
            assert!(!enabled_in(&format!("{}=maybe\n", f.key()), f), "{f:?}");
            assert!(enabled_in(&format!("{}=1\n", f.key()), f), "{f:?}");
            assert!(!enabled_in(&format!("{}=0\n", f.key()), f), "{f:?}");
            assert_eq!(f.log().feature(), f);
        }
        // Each switch is its own.
        assert!(!enabled_in("log_graphics=1\n", Feature::Translation));
    }

    #[test]
    fn the_switch_is_saved_with_the_other_lines() {
        let t = settings::with_key("check_updates=false\n", Feature::Textures.key(), "1", None);
        assert!(enabled_in(&t, Feature::Textures) && !enabled_in(&t, Feature::Graphics));
        assert!(t.starts_with("check_updates=false\n"));
        let t = settings::with_key(&t, Feature::Textures.key(), "0", None);
        assert!(!enabled_in(&t, Feature::Textures));
    }

    #[test]
    fn off_writes_no_file_and_on_writes_live() {
        let dir = scratch("live");
        let path = dir.join("x_log.txt");
        let log = FeatureLog::new(Feature::Graphics);
        log.init_with(&path, false);
        log.write("hello");
        log.write("could not open the pack");
        assert!(!path.exists(), "a log that is off writes nothing");
        assert_eq!(log.recent().len(), 1, "problems are kept in memory");
        log.set_enabled(true);
        log.write("first");
        log.set_enabled(false);
        log.write("hidden");
        log.set_enabled(true);
        log.write("second");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("first") && text.contains("second") && !text.contains("hidden"), "{text}");
        assert!(!text.contains("hello"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_previous_session_is_kept() {
        let dir = scratch("prev");
        let path = dir.join("y_log.txt");
        std::fs::write(&path, "old session\n").unwrap();
        let log = FeatureLog::new(Feature::Host);
        log.init_with(&path, true);
        log.write("new session");
        assert_eq!(std::fs::read_to_string(dir.join("y_log.prev.txt")).unwrap(), "old session\n");
        assert!(std::fs::read_to_string(&path).unwrap().contains("new session"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_memory_ring_is_bounded() {
        let log = FeatureLog::new(Feature::Translation);
        for i in 0..RECENT + 10 {
            log.write(&format!("error {i}"));
        }
        let r = log.recent();
        assert_eq!(r.len(), RECENT);
        assert!(r.last().unwrap().ends_with(&format!("error {}", RECENT + 9)));
        assert!(is_problem("Failed to hook") && is_problem("cannot create") && !is_problem("hooked 3 functions"));
    }
}
