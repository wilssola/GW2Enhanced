//! `config.ini` in `key=value` format, with `#` for comments.
//!
//! The file is rewritten in its current form on every read: the known keys
//! in order and with the comments from the code, with the player's values, and
//! the unknown keys preserved at the end. That way a new comment or option
//! is never left out.

use std::fs;
use std::io;
use std::path::Path;

/// Participation in sending texts to improve the translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contribute {
    /// Has not answered yet: the overlay asks.
    Ask,
    Yes,
    No,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Translation language: folder name in `languages\` (only `pt-BR` for now).
    pub language: String,
    /// Apply the translation.
    pub translate: bool,
    /// Write seen strings to `discovered.jsonl`.
    pub autocollect: bool,
    /// Decrypt RC4 strings with the server's `keySeed` and write `decrypted.jsonl`.
    pub seed_decode: bool,
    /// Live-translate whatever is missing, via the local sidecar.
    pub live_translate: bool,
    /// Reapply translations to text already displayed (automatically and via the key).
    pub live_refresh: bool,
    /// Virtual key code for the manual refresh (default F10).
    pub refresh_key: u32,
    /// Show the overlay.
    pub overlay: bool,
    /// Key that opens and closes the overlay (default Insert).
    pub overlay_key: u32,
    /// Send untranslated game texts to improve the project.
    pub contribute: Contribute,
    /// Longest extra time (seconds) a speech bubble stays up while its
    /// translation is being made; 0 = never hold.
    pub bubble_wait: u32,
    /// Where the local translator runs.
    pub translator_device: Device,
    /// CPU threads of the local translator; 0 = chosen from the CPU.
    pub translator_threads: u32,
    /// The first-run setup in the overlay was completed.
    pub onboarded: bool,
}

/// Where the local translator runs. `Auto` tries the GPU (CUDA on NVIDIA,
/// DirectML on any card) when there is free video memory, else the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Auto,
    Cuda,
    DirectMl,
    Cpu,
}

impl Device {
    pub const ALL: [Device; 4] = [Device::Auto, Device::Cuda, Device::DirectMl, Device::Cpu];

    pub fn key(self) -> &'static str {
        match self {
            Device::Auto => "auto",
            Device::Cuda => "cuda",
            Device::DirectMl => "directml",
            Device::Cpu => "cpu",
        }
    }

    pub fn parse(v: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.key().eq_ignore_ascii_case(v.trim()))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: "pt-BR".into(),
            translate: true,
            autocollect: true,
            seed_decode: true,
            live_translate: true,
            live_refresh: true,
            refresh_key: 0x79,
            overlay: true,
            overlay_key: 0x2D,
            contribute: Contribute::Ask,
            bubble_wait: 8,
            translator_device: Device::Auto,
            translator_threads: 0,
            onboarded: false,
        }
    }
}

const KEYS: &[(&str, &str)] = &[
    ("language", "Translation language (folder in languages\\)"),
    ("translate", "Apply the translation"),
    ("autocollect", "Collect new texts into discovered.jsonl"),
    ("seed_decode", "Decrypt protected texts and write decrypted.jsonl"),
    (
        "live_translate",
        "Translate on your PC whatever is missing from the dictionary",
    ),
    (
        "live_refresh",
        "Reapply translations to text already on screen (automatically and via the key)",
    ),
    ("refresh_key", "Manual refresh key (virtual code; 0x79 = F10)"),
    ("overlay", "Show the GW2 Translated overlay"),
    ("overlay_key", "Overlay key (virtual code; 0x2D = Insert)"),
    (
        "contribute",
        "Send game texts and local translations to improve the project (ask, true, false)",
    ),
    (
        "bubble_wait",
        "Most seconds a speech bubble may stay up longer while its translation is made; longer lines wait longer (0 = off)",
    ),
    (
        "translator_device",
        "Where the local translator runs: auto, cuda (NVIDIA), directml (any GPU) or cpu; falls back to cpu",
    ),
    (
        "translator_threads",
        "CPU threads of the local translator (0 = automatic)",
    ),
    ("onboarded", "First-run setup completed (false shows it again)"),
];

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "sim" => Some(true),
        "0" | "false" | "no" | "off" | "nao" | "não" => Some(false),
        _ => None,
    }
}

fn parse_u32(v: &str) -> Option<u32> {
    let v = v.trim();
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

fn parse_contribute(v: &str) -> Option<Contribute> {
    match parse_bool(v) {
        Some(true) => Some(Contribute::Yes),
        Some(false) => Some(Contribute::No),
        None => v.trim().eq_ignore_ascii_case("ask").then_some(Contribute::Ask),
    }
}

/// Uncommented `key=value` pairs, in file order.
fn pairs(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|l| {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            return None;
        }
        let (k, v) = l.split_once('=')?;
        Some((k.trim(), v.trim()))
    })
}

impl Config {
    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        for (k, v) in pairs(text) {
            c.apply(k, v);
        }
        c
    }

    /// Applies one `key=value`; invalid values are ignored.
    fn apply(&mut self, k: &str, v: &str) {
        match k {
            "language" if crate::paths::valid_language(v) => self.language = v.to_owned(),
            "translate" => self.translate = parse_bool(v).unwrap_or(self.translate),
            "autocollect" => self.autocollect = parse_bool(v).unwrap_or(self.autocollect),
            "seed_decode" => self.seed_decode = parse_bool(v).unwrap_or(self.seed_decode),
            "live_translate" => self.live_translate = parse_bool(v).unwrap_or(self.live_translate),
            "live_refresh" => self.live_refresh = parse_bool(v).unwrap_or(self.live_refresh),
            "refresh_key" => self.refresh_key = parse_u32(v).unwrap_or(self.refresh_key),
            "overlay" => self.overlay = parse_bool(v).unwrap_or(self.overlay),
            "overlay_key" => self.overlay_key = parse_u32(v).unwrap_or(self.overlay_key),
            "contribute" => self.contribute = parse_contribute(v).unwrap_or(self.contribute),
            "bubble_wait" => self.bubble_wait = parse_u32(v).map_or(self.bubble_wait, |s| s.min(30)),
            "translator_device" => self.translator_device = Device::parse(v).unwrap_or(self.translator_device),
            "translator_threads" => {
                self.translator_threads = parse_u32(v).map_or(self.translator_threads, |n| n.min(64))
            }
            "onboarded" => self.onboarded = parse_bool(v).unwrap_or(self.onboarded),
            _ => {}
        }
    }

    fn value_of(&self, key: &str) -> String {
        match key {
            "language" => self.language.clone(),
            "translate" => self.translate.to_string(),
            "autocollect" => self.autocollect.to_string(),
            "seed_decode" => self.seed_decode.to_string(),
            "live_translate" => self.live_translate.to_string(),
            "live_refresh" => self.live_refresh.to_string(),
            "refresh_key" => format!("0x{:X}", self.refresh_key),
            "overlay" => self.overlay.to_string(),
            "overlay_key" => format!("0x{:X}", self.overlay_key),
            "contribute" => match self.contribute {
                Contribute::Ask => "ask".into(),
                Contribute::Yes => "true".into(),
                Contribute::No => "false".into(),
            },
            "bubble_wait" => self.bubble_wait.to_string(),
            "translator_device" => self.translator_device.key().into(),
            "translator_threads" => self.translator_threads.to_string(),
            "onboarded" => self.onboarded.to_string(),
            _ => unreachable!(),
        }
    }

    /// Full text of `config.ini`, with the unknown keys at the end.
    fn render_with(&self, unknown: &[(&str, &str)]) -> String {
        let mut s = String::from("# GW2 Translated — configuration\n");
        for (k, help) in KEYS {
            s.push_str(&format!("\n# {help}\n{k}={}\n", self.value_of(k)));
        }
        if !unknown.is_empty() {
            s.push_str("\n# Other keys (not used by this version)\n");
            for (k, v) in unknown {
                s.push_str(&format!("{k}={v}\n"));
            }
        }
        s
    }

    pub fn render(&self) -> String {
        self.render_with(&[])
    }

    /// Reads the file (or uses the defaults) and rewrites it in its current form.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let c = Self::parse(&text);
        let known: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
        let unknown: Vec<(&str, &str)> = pairs(&text).filter(|(k, _)| !known.contains(k)).collect();
        let out = c.render_with(&unknown);
        if out != text {
            fs::write(path, out)?;
        }
        Ok(c)
    }

    /// Changes one key in the file (the rest stays as it is).
    pub fn save_key(path: &Path, key: &str, value: &str) -> io::Result<Self> {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut c = Self::parse(&text);
        c.apply(key, value);
        let known: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
        let unknown: Vec<(&str, &str)> = pairs(&text).filter(|(k, _)| !known.contains(k)).collect();
        fs::write(path, c.render_with(&unknown))?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_keys_and_defaults_the_rest() {
        let c = Config::parse(
            "translate=false\nautocollect=true\n# x=1\nseed_decode=0\nrefresh_key=0x78\ncontribute=yes\n",
        );
        assert!(!c.translate);
        assert!(c.autocollect);
        assert!(!c.seed_decode);
        assert_eq!(c.refresh_key, 0x78);
        assert_eq!(c.contribute, Contribute::Yes);
        assert!(c.live_translate, "missing key uses the default");
    }

    #[test]
    fn bad_values_fall_back_to_default() {
        let c = Config::parse("translate=talvez\nrefresh_key=zz\nlanguage=..\\x\ncontribute=maybe");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn render_roundtrips() {
        let c = Config {
            translate: false,
            refresh_key: 0x7A,
            contribute: Contribute::No,
            ..Default::default()
        };
        assert_eq!(Config::parse(&c.render()), c);
    }

    #[test]
    fn load_rewrites_old_comments_and_keeps_values_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.ini");
        fs::write(
            &p,
            "# Nome antigo — configuração\n\n# comentário velho\ntranslate=false\nfoo=bar\n",
        )
        .unwrap();
        let c = Config::load_or_create(&p).unwrap();
        assert!(!c.translate);
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# GW2 Translated — configuration\n"));
        assert!(!text.contains("comentário velho") && !text.contains("Nome antigo"));
        assert!(text.contains("\nfoo=bar\n") && text.contains("\noverlay_key=0x2D\n"));
        assert_eq!(Config::parse(&text), c);
    }

    #[test]
    fn save_key_changes_only_that_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.ini");
        fs::write(&p, "translate=false\nfoo=bar\n").unwrap();
        let c = Config::save_key(&p, "contribute", "true").unwrap();
        assert_eq!(c.contribute, Contribute::Yes);
        assert!(!c.translate);
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("\ncontribute=true\n") && text.contains("\nfoo=bar\n"));
    }
}
