//! Language of the overlay (the mod's own interface), shared by every feature panel.
//!
//! This is not the language the game is translated into (that is the translation feature's own `language` setting):
//! it only picks the words of the overlay. It is stored as `ui_language=en|pt-BR` in `GW2Enhanced\config.ini` next to
//! the game; without that key it follows the Windows display language (Portuguese of any region: `pt-BR`, else
//! English).
//!
//! Strings live where they are drawn, as pairs: [`tr`] for plain text and [`trf!`](crate::trf) for formatted text.
//! Both take every language as an argument, so a string without its translation does not compile.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::settings;

/// A language the overlay is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    PtBr,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::PtBr];

    /// The value written to `config.ini`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::PtBr => "pt-BR",
        }
    }

    /// The name shown in the selector, in the language itself.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::PtBr => "Português (Brasil)",
        }
    }

    /// A `config.ini` value or a locale name (`en`, `en-US`, `pt-BR`, `pt_PT`...); `None` when it is no language we
    /// have.
    pub fn parse(v: &str) -> Option<Lang> {
        let v = v.trim().to_ascii_lowercase();
        let primary = v.split(['-', '_']).next().unwrap_or("");
        match primary {
            "en" => Some(Lang::En),
            "pt" => Some(Lang::PtBr),
            _ => None,
        }
    }

    /// The language for a Windows LANGID: Portuguese (any region) is `PtBr`, anything else English.
    pub fn from_langid(langid: u16) -> Lang {
        const LANG_PORTUGUESE: u16 = 0x16;
        if langid & 0x3ff == LANG_PORTUGUESE { Lang::PtBr } else { Lang::En }
    }

    fn index(self) -> u8 {
        match self {
            Lang::En => 0,
            Lang::PtBr => 1,
        }
    }

    fn from_index(i: u8) -> Lang {
        if i == 1 { Lang::PtBr } else { Lang::En }
    }
}

/// The config key.
pub const KEY: &str = "ui_language";

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The overlay language in use. English until [`init`] runs.
pub fn current() -> Lang {
    Lang::from_index(CURRENT.load(Ordering::Relaxed))
}

/// Switches the overlay language for this session (not saved; see [`choose`]).
pub fn set(lang: Lang) {
    CURRENT.store(lang.index(), Ordering::Relaxed);
}

/// The text for the current language.
pub fn tr(en: &'static str, pt_br: &'static str) -> &'static str {
    pick(current(), en, pt_br)
}

/// The text for `lang`.
pub fn pick<T>(lang: Lang, en: T, pt_br: T) -> T {
    match lang {
        Lang::En => en,
        Lang::PtBr => pt_br,
    }
}

/// `format!` in the current language: `trf!("{n} files", "{n} arquivos")`, with the same arguments for both.
#[macro_export]
macro_rules! trf {
    ($en:literal, $pt_br:literal $(, $($arg:tt)*)?) => {
        match $crate::i18n::current() {
            $crate::i18n::Lang::En => format!($en $(, $($arg)*)?),
            $crate::i18n::Lang::PtBr => format!($pt_br $(, $($arg)*)?),
        }
    };
}

/// `ui_language` in a `config.ini` text (`key=value` lines, `#`/`;` comments); `None` when absent or unknown.
pub fn from_config(text: &str) -> Option<Lang> {
    settings::get(text, KEY).and_then(|v| Lang::parse(&v))
}

const COMMENT: &str = "Language of the overlay: en or pt-BR (also changed in the overlay)";

/// `text` with `ui_language` set to `lang`: the existing line is replaced, otherwise one is added at the end. Every
/// other line is kept as it was.
pub fn with_language(text: &str, lang: Lang) -> String {
    settings::with_key(text, KEY, lang.code(), Some(COMMENT))
}

/// The saved choice, else the Windows display language.
pub fn resolve(config: Option<&str>, system: Lang) -> Lang {
    config.and_then(from_config).unwrap_or(system)
}

/// The Windows display language as an overlay language.
pub fn system_language() -> Lang {
    #[cfg(windows)]
    {
        let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        Lang::from_langid(id)
    }
    #[cfg(not(windows))]
    Lang::En
}

/// Reads the saved choice (or the Windows language) once at start.
pub fn init() {
    set(resolve(Some(&settings::read()), system_language()));
}

/// The player picked `lang` in the overlay: used right away and saved.
pub fn choose(lang: Lang) -> std::io::Result<()> {
    set(lang);
    settings::save(KEY, lang.code(), Some(COMMENT))
}

/// Writes `lang` into the config file at `path` (created when missing), keeping its other lines.
pub fn save_to(path: &Path, lang: Lang) -> std::io::Result<()> {
    settings::save_to(path, KEY, lang.code(), Some(COMMENT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes_and_locales() {
        assert_eq!(Lang::parse("en"), Some(Lang::En));
        assert_eq!(Lang::parse(" EN-us "), Some(Lang::En));
        assert_eq!(Lang::parse("pt-BR"), Some(Lang::PtBr));
        assert_eq!(Lang::parse("pt_PT"), Some(Lang::PtBr));
        assert_eq!(Lang::parse("pt"), Some(Lang::PtBr));
        assert_eq!(Lang::parse("de"), None);
        assert_eq!(Lang::parse(""), None);
        for l in Lang::ALL {
            assert_eq!(Lang::parse(l.code()), Some(l));
            assert!(!l.name().is_empty());
        }
    }

    #[test]
    fn windows_portuguese_of_any_region_is_pt_br() {
        assert_eq!(Lang::from_langid(0x0416), Lang::PtBr); // pt-BR
        assert_eq!(Lang::from_langid(0x0816), Lang::PtBr); // pt-PT
        assert_eq!(Lang::from_langid(0x0409), Lang::En); // en-US
        assert_eq!(Lang::from_langid(0x0C0A), Lang::En); // es-ES
        assert_eq!(Lang::from_langid(0x0407), Lang::En); // de-DE
    }

    #[test]
    fn saved_choice_wins_over_the_system_language() {
        assert_eq!(resolve(None, Lang::PtBr), Lang::PtBr);
        assert_eq!(resolve(Some(""), Lang::En), Lang::En);
        assert_eq!(resolve(Some("check_updates=false\n"), Lang::PtBr), Lang::PtBr);
        assert_eq!(resolve(Some("ui_language=en\n"), Lang::PtBr), Lang::En);
        assert_eq!(resolve(Some("UI_LANGUAGE = pt-BR\n"), Lang::En), Lang::PtBr);
        assert_eq!(resolve(Some("# ui_language=en\n"), Lang::PtBr), Lang::PtBr);
        assert_eq!(resolve(Some("ui_language=klingon\n"), Lang::PtBr), Lang::PtBr);
    }

    #[test]
    fn writing_keeps_the_other_lines() {
        let t = with_language("# comment\ncheck_updates=false\n", Lang::PtBr);
        assert_eq!(t, format!("# comment\ncheck_updates=false\n# {COMMENT}\nui_language=pt-BR\n"));
        let t = with_language(&t, Lang::En);
        assert_eq!(t, format!("# comment\ncheck_updates=false\n# {COMMENT}\nui_language=en\n"));
        // Duplicates collapse into one line, at the first one's place; a commented one stays a comment.
        let t = with_language("ui_language=en\n# ui_language=en\nx=1\nui_language=pt-BR\n", Lang::PtBr);
        assert_eq!(t, "ui_language=pt-BR\n# ui_language=en\nx=1\n");
        assert_eq!(from_config(&with_language("", Lang::PtBr)), Some(Lang::PtBr));
    }

    #[test]
    fn saves_and_reads_back_from_a_file() {
        let dir = std::env::temp_dir().join(format!("gw2e_i18n_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("GW2Enhanced").join("config.ini");
        save_to(&path, Lang::PtBr).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(resolve(Some(&text), Lang::En), Lang::PtBr);
        std::fs::write(&path, "check_updates=false\nui_language=pt-BR\n").unwrap();
        save_to(&path, Lang::En).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "check_updates=false\nui_language=en\n");
        assert_eq!(resolve(Some(&text), Lang::PtBr), Lang::En);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strings_follow_the_current_language() {
        // The only test that touches the global.
        let n = 3;
        set(Lang::PtBr);
        assert_eq!(tr("Save", "Salvar"), "Salvar");
        assert_eq!(trf!("{n} files", "{n} arquivos"), "3 arquivos");
        assert_eq!(trf!("{} of {}", "{} de {}", 1, 2), "1 de 2");
        set(Lang::En);
        assert_eq!(tr("Save", "Salvar"), "Save");
        assert_eq!(trf!("{n} files", "{n} arquivos"), "3 files");
        assert_eq!(pick(Lang::PtBr, 1, 2), 2);
    }
}
