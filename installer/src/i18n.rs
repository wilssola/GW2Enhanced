//! The installer's two languages on the Rust side: Brazilian Portuguese and English.
//!
//! The messages the window and the command line show (errors, install log lines, launch results) are written in both,
//! next to each other, with [`tr!`]: `tr!("pt-BR text {x}", "English text {x}")`. Which one is used is a process-wide
//! choice: the `language` of the settings, else the one the page runs in, else the system's.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// `"pt-BR"` for any Portuguese tag, `"en"` for any English one, `None` for anything else (an empty setting too).
pub fn normalize(lang: &str) -> Option<&'static str> {
    let l = lang.trim().to_ascii_lowercase().replace('_', "-");
    if l == "pt" || l.starts_with("pt-") {
        Some("pt-BR")
    } else if l == "en" || l.starts_with("en-") {
        Some("en")
    } else {
        None
    }
}

/// The language in use: `"pt-BR"` or `"en"`.
pub fn current() -> &'static str {
    if ENGLISH.load(Ordering::Relaxed) { "en" } else { "pt-BR" }
}

pub fn is_en() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Switches to `lang` when it is Portuguese or English; anything else leaves the current language.
pub fn set(lang: &str) {
    if let Some(l) = normalize(lang) {
        ENGLISH.store(l == "en", Ordering::Relaxed);
    }
}

/// The saved choice (`Settings::language`), else the system's language (Portuguese, else English).
pub fn init(saved: &str) {
    let l = normalize(saved).unwrap_or_else(system_language);
    ENGLISH.store(l == "en", Ordering::Relaxed);
}

/// `pt` or `en`, by the language in use.
pub fn pick<'a>(pt: &'a str, en: &'a str) -> &'a str {
    if is_en() { en } else { pt }
}

/// Windows' display language: Portuguese (primary language 0x16) or English.
#[cfg(windows)]
pub fn system_language() -> &'static str {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    let id = unsafe { GetUserDefaultUILanguage() };
    if id & 0x3ff == 0x16 { "pt-BR" } else { "en" }
}

/// `LC_ALL`, `LC_MESSAGES`, `LANG` (`pt_BR.UTF-8`...): Portuguese or English.
#[cfg(not(windows))]
pub fn system_language() -> &'static str {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .and_then(|v| normalize(v.split(['.', '@']).next().unwrap_or("")))
        .unwrap_or("en")
}

/// A message in the language in use: `tr!("Portuguese {x}", "English {x}", args...)`, formatted like `format!`.
#[macro_export]
macro_rules! tr {
    ($pt:tt, $en:tt $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() { format!($en $(, $arg)*) } else { format!($pt $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_language_tags() {
        assert_eq!(normalize("pt-BR"), Some("pt-BR"));
        assert_eq!(normalize("pt_PT"), Some("pt-BR"));
        assert_eq!(normalize("PT"), Some("pt-BR"));
        assert_eq!(normalize("en-US"), Some("en"));
        assert_eq!(normalize("en"), Some("en"));
        assert_eq!(normalize("de-DE"), None);
        assert_eq!(normalize(""), None);
    }

    /// Every `data-i18n` / `data-i18n-attr` key of the page has its English in app.js.
    #[test]
    fn ui_strings_have_english() {
        let html = include_str!("../ui/index.html");
        let js = include_str!("../ui/app.js");
        let attr = |name: &str| -> Vec<String> {
            let pat = format!(" {name}=\"");
            html.split(pat.as_str()).skip(1).map(|rest| rest[..rest.find('"').unwrap()].to_string()).collect()
        };
        let mut keys = attr("data-i18n");
        for v in attr("data-i18n-attr") {
            for pair in v.split_whitespace() {
                let (_, key) = pair.split_once(':').expect("data-i18n-attr is attr:key");
                keys.push(key.to_string());
            }
        }
        assert!(keys.len() > 50, "data-i18n markers missing from index.html");
        let at = js.find("\n  en: {").expect("STR.en in app.js");
        let (pt, en) = (&js[js.find("\"pt-BR\": {").expect("STR[\"pt-BR\"] in app.js")..at], &js[at..]);
        for k in &keys {
            assert!(en.contains(&format!("\"{k}\":")), "no English for {k} in app.js");
        }
        // Keys the script itself uses, t("key"...): in both languages.
        let used: Vec<&str> = js
            .match_indices("t(\"")
            .filter(|(i, _)| !js[..*i].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.'))
            .map(|(i, _)| {
                let rest = &js[i + 3..];
                &rest[..rest.find('"').unwrap()]
            })
            .collect();
        assert!(used.len() > 60, "t(\"...\") calls missing from app.js");
        for k in used.iter().filter(|k| !k.ends_with('.')) {
            assert!(pt.contains(&format!("\"{k}\":")), "no Portuguese for {k} in app.js");
            assert!(en.contains(&format!("\"{k}\":")), "no English for {k} in app.js");
        }
    }
}
