//! `GW2Enhanced\config.ini` next to the game: the settings shared by the whole mod (the overlay language, the logs,
//! the update check). `key=value` lines, `#`/`;` comments; writing one key keeps every other line as it was.

use std::path::{Path, PathBuf};

/// The value of `key` in an ini text (the first one; keys ignore case).
pub fn get(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim().to_string())
}

/// `text` with `key` set to `value`: the first line with the key is replaced (later duplicates are dropped), otherwise
/// the line is added at the end, under `comment` when one is given. Every other line is kept.
pub fn with_key(text: &str, key: &str, value: &str, comment: Option<&str>) -> String {
    let line = format!("{key}={value}");
    let mut found = false;
    let mut out: Vec<String> = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        let is_key = !t.starts_with('#')
            && !t.starts_with(';')
            && t.split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key));
        if is_key {
            if !found {
                out.push(line.clone());
                found = true;
            }
        } else {
            out.push(l.to_string());
        }
    }
    if !found {
        if let Some(c) = comment {
            out.push(format!("# {c}"));
        }
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// A boolean value: `1`, `true`, `yes`, `on` (any case) are true; `0`, `false`, `no`, `off` false; anything else
/// `None`.
pub fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `GW2Enhanced\config.ini` next to the running exe.
pub fn path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("GW2Enhanced").join("config.ini"))
}

/// The text of the shared config (empty when it does not exist yet).
pub fn read() -> String {
    path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default()
}

/// Writes `key=value` into the file at `path` (created with its folder when missing), keeping its other lines.
pub fn save_to(path: &Path, key: &str, value: &str, comment: Option<&str>) -> std::io::Result<()> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, with_key(&old, key, value, comment))
}

/// Writes `key=value` into the shared config.
pub fn save(key: &str, value: &str, comment: Option<&str>) -> std::io::Result<()> {
    match path() {
        Some(p) => save_to(&p, key, value, comment),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_keys_ignoring_comments_and_case() {
        let t = "# x=1\n; x=2\n X = 3 \ny=a=b\n";
        assert_eq!(get(t, "x").as_deref(), Some("3"));
        assert_eq!(get(t, "Y").as_deref(), Some("a=b"));
        assert_eq!(get(t, "z"), None);
    }

    #[test]
    fn writing_one_key_keeps_the_others() {
        let t = with_key("# comment\ncheck_updates=false\n", "log_host", "1", Some("Logs"));
        assert_eq!(t, "# comment\ncheck_updates=false\n# Logs\nlog_host=1\n");
        let t = with_key(&t, "log_host", "0", Some("Logs"));
        assert_eq!(t, "# comment\ncheck_updates=false\n# Logs\nlog_host=0\n");
        let t = with_key("a=1\n# a=1\nb=2\nA=3\n", "a", "9", None);
        assert_eq!(t, "a=9\n# a=1\nb=2\n");
        assert_eq!(with_key("", "k", "v", None), "k=v\n");
    }

    #[test]
    fn booleans() {
        for v in ["1", "true", "YES", " on "] {
            assert_eq!(parse_bool(v), Some(true), "{v}");
        }
        for v in ["0", "False", "no", "OFF"] {
            assert_eq!(parse_bool(v), Some(false), "{v}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }
}
