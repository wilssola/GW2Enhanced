//! Making Wine / Proton load our `msimg32.dll` (see docs/launch-methods.md, "Linux").
//!
//! `msimg32` is one of Wine's own (builtin) DLLs, and Wine prefers its builtin copy to a DLL of the same name in the
//! game folder. The override `msimg32=n,b` (native first, then builtin) makes it load ours, which then forwards to
//! the real one. For Steam it goes in the game's launch options:
//!
//! ```text
//! WINEDLLOVERRIDES="msimg32=n,b" %command%
//! ```
//!
//! Steam keeps each account's launch options in `<Steam>/userdata/<account>/config/localconfig.vdf`
//! (`UserLocalConfigStore/Software/Valve/Steam/apps/1284210/LaunchOptions`) and rewrites that file while it runs,
//! so it is only edited with Steam closed, after a backup. The same override set in the Proton prefix
//! (`winecfg` > Libraries, stored in `pfx/user.reg`) works too and is recognised.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::games::{GameInstall, STEAM_APP_ID, library_of};
use crate::vdf;

pub const DLL_OVERRIDE: &str = "msimg32=n,b";
/// What to paste into Steam's launch options.
pub const LAUNCH_LINE: &str = "WINEDLLOVERRIDES=\"msimg32=n,b\" %command%";
const LOCALCONFIG_BACKUP: &str = "localconfig.vdf.antes-do-gw2enhanced";

fn launch_options_path() -> [&'static str; 7] {
    ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", STEAM_APP_ID, "LaunchOptions"]
}

/// The value of `WINEDLLOVERRIDES=` in a launch line (quoted or not), with where it sits.
fn overrides_value(line: &str) -> Option<(String, std::ops::Range<usize>)> {
    let start = line.find("WINEDLLOVERRIDES=")? + "WINEDLLOVERRIDES=".len();
    let rest = &line[start..];
    if let Some(q) = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\'')) {
        let quote = rest.as_bytes()[0] as char;
        let end = q.find(quote).unwrap_or(q.len());
        Some((q[..end].to_string(), start..(start + 1 + end + 1).min(line.len())))
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some((rest[..end].to_string(), start..start + end))
    }
}

fn names_msimg32(names: &str) -> bool {
    names.split(',').any(|n| {
        let n = n.trim().trim_start_matches('*').to_ascii_lowercase();
        n == "msimg32" || n == "msimg32.dll"
    })
}

/// The overrides `msimg32` native first (`n` or `n,b`).
pub fn has_override(line: &str) -> bool {
    let Some((value, _)) = overrides_value(line) else { return false };
    value.split(';').any(|entry| {
        entry.split_once('=').is_some_and(|(names, mode)| names_msimg32(names) && mode.trim().starts_with('n'))
    })
}

/// `line` with the override added, keeping everything else (other overrides, arguments).
pub fn with_override(line: &str) -> String {
    let line = line.trim();
    if has_override(line) {
        return line.to_string();
    }
    if let Some((value, span)) = overrides_value(line) {
        let mut entries: Vec<String> = vec![DLL_OVERRIDE.to_string()];
        for entry in value.split(';').filter(|e| !e.trim().is_empty()) {
            match entry.split_once('=') {
                Some((names, mode)) if names_msimg32(names) => {
                    let others: Vec<&str> = names.split(',').filter(|n| !names_msimg32(n)).collect();
                    if !others.is_empty() {
                        entries.push(format!("{}={mode}", others.join(",")));
                    }
                }
                _ => entries.push(entry.to_string()),
            }
        }
        return format!("{}\"{}\"{}", &line[..span.start], entries.join(";"), &line[span.end..]);
    }
    if line.is_empty() {
        LAUNCH_LINE.to_string()
    } else if line.contains("%command%") {
        format!("WINEDLLOVERRIDES=\"{DLL_OVERRIDE}\" {line}")
    } else {
        // Without %command%, Steam passes the whole line to the game as arguments.
        format!("{LAUNCH_LINE} {line}")
    }
}

/// The prefix's registry (`user.reg`) overrides `msimg32` to native (winecfg > Libraries).
pub fn prefix_has_override(user_reg: &str) -> bool {
    let mut in_section = false;
    for line in user_reg.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.to_ascii_lowercase().starts_with("[software\\\\wine\\\\dlloverrides]");
            continue;
        }
        if in_section
            && let Some((k, v)) = line.split_once('=')
        {
            let k = k.trim().trim_matches('"').trim_start_matches('*');
            let v = v.trim().trim_matches('"');
            if k.eq_ignore_ascii_case("msimg32") && v.starts_with('n') {
                return true;
            }
        }
    }
    false
}

/// The launch options of the game in a `localconfig.vdf`.
pub fn read_launch_options(localconfig: &str) -> Option<String> {
    vdf::parse(localconfig).ok()?.path(&launch_options_path()).and_then(vdf::Value::str).map(str::to_string)
}

/// Every Steam account's `localconfig.vdf` under a Steam folder.
pub fn localconfig_files(steam_root: &Path) -> Vec<PathBuf> {
    let Ok(users) = std::fs::read_dir(steam_root.join("userdata")) else { return Vec::new() };
    let mut out: Vec<PathBuf> = users
        .flatten()
        .filter(|u| u.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) && u.file_name() != "0")
        .map(|u| u.path().join("config").join("localconfig.vdf"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// The Proton prefix of the game (`<library>/steamapps/compatdata/1284210/pfx`).
pub fn prefix_of(game: &Path) -> Option<PathBuf> {
    Some(library_of(game)?.join("steamapps").join("compatdata").join(STEAM_APP_ID).join("pfx"))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProtonStatus {
    /// The Proton prefix, when the game ran once (Steam creates it then).
    pub prefix: Option<String>,
    /// The launch options of each Steam account found (empty when there are none).
    pub launch_options: Vec<String>,
    /// Some account's launch options, or the prefix, have the override.
    pub override_set: bool,
    /// The line to paste into Steam (the current options with the override added).
    pub line: String,
    pub steam_running: bool,
    /// Accounts whose launch options this can edit.
    pub accounts: usize,
}

/// The Proton state of a Steam install (Linux).
pub fn status(game: &GameInstall) -> ProtonStatus {
    let prefix = prefix_of(&game.path).filter(|p| p.is_dir());
    let prefix_ok = prefix
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p.join("user.reg")).ok())
        .is_some_and(|r| prefix_has_override(&r));
    let files = game.steam_root.as_deref().map(localconfig_files).unwrap_or_default();
    let options: Vec<String> = files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .map(|t| read_launch_options(&t).unwrap_or_default())
        .collect();
    let current = options.iter().find(|o| !o.trim().is_empty()).cloned().unwrap_or_default();
    ProtonStatus {
        prefix: prefix.map(|p| p.display().to_string()),
        override_set: prefix_ok || options.iter().any(|o| has_override(o)),
        line: with_override(&current),
        launch_options: options,
        steam_running: crate::launch::steam_running(),
        accounts: files.len(),
    }
}

/// Adds the override to the game's launch options of every Steam account (Steam must be closed).
pub fn apply(game: &GameInstall) -> Result<Vec<String>, String> {
    if crate::launch::steam_running() {
        return Err(tr!(
            "feche a Steam antes (ela regrava as opções de inicialização enquanto está aberta)",
            "close Steam first (it rewrites the launch options while it is open)"
        ));
    }
    let root = game
        .steam_root
        .as_deref()
        .ok_or_else(|| tr!("não encontrei a pasta da Steam desta instalação", "could not find the Steam folder of this install"))?;
    let files = localconfig_files(root);
    if files.is_empty() {
        return Err(tr!(
            "não encontrei as configurações das contas da Steam (userdata); abra a Steam e entre na conta uma vez",
            "could not find the Steam account settings (userdata); open Steam and sign in once"
        ));
    }
    let mut log = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|e| tr!("não consegui ler {}: {e}", "could not read {}: {e}", file.display()))?;
        let current = read_launch_options(&text).unwrap_or_default();
        if has_override(&current) {
            log.push(tr!("{}: já estava configurado", "{}: already set", account_of(&file)));
            continue;
        }
        let new_text = vdf::set(&text, &launch_options_path(), &with_override(&current))?;
        let backup = file.with_file_name(LOCALCONFIG_BACKUP);
        if !backup.exists() {
            std::fs::copy(&file, &backup).map_err(|e| tr!("não consegui fazer a cópia de segurança: {e}", "could not make the backup: {e}"))?;
        }
        // Written next to it, then renamed: a half-written file never replaces Steam's.
        let tmp = file.with_extension("vdf.gw2enhanced-tmp");
        std::fs::write(&tmp, new_text).map_err(|e| tr!("não consegui gravar {}: {e}", "could not write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &file).map_err(|e| tr!("não consegui gravar {}: {e}", "could not write {}: {e}", file.display()))?;
        log.push(tr!(
            "{}: opções de inicialização atualizadas (cópia em {LOCALCONFIG_BACKUP})",
            "{}: launch options updated (backup in {LOCALCONFIG_BACKUP})",
            account_of(&file)
        ));
    }
    Ok(log)
}

fn account_of(localconfig: &Path) -> String {
    let id = localconfig.parent().and_then(Path::parent).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
    tr!("conta {}", "account {}", id.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_is_recognised_in_its_usual_forms() {
        assert!(has_override(LAUNCH_LINE));
        assert!(has_override("WINEDLLOVERRIDES=\"d3d11=n,b;msimg32=n,b\" %command%"));
        assert!(has_override("WINEDLLOVERRIDES='msimg32,winmm=n' %command% -provider Portal"));
        assert!(has_override("PROTON_LOG=1 WINEDLLOVERRIDES=msimg32.dll=n,b %command%"));
        assert!(!has_override("WINEDLLOVERRIDES=\"msimg32=b,n\" %command%"));
        assert!(!has_override("WINEDLLOVERRIDES=\"d3d11=n,b\" %command%"));
        assert!(!has_override("-provider Portal"));
        assert!(!has_override(""));
    }

    #[test]
    fn override_is_added_keeping_the_rest() {
        assert_eq!(with_override(""), LAUNCH_LINE);
        assert_eq!(with_override("  "), LAUNCH_LINE);
        assert_eq!(with_override("-provider Portal"), format!("{LAUNCH_LINE} -provider Portal"));
        assert_eq!(with_override("gamemoderun %command% -shareArchive"), "WINEDLLOVERRIDES=\"msimg32=n,b\" gamemoderun %command% -shareArchive");
        assert_eq!(
            with_override("WINEDLLOVERRIDES=\"d3d11=n,b\" %command%"),
            "WINEDLLOVERRIDES=\"msimg32=n,b;d3d11=n,b\" %command%"
        );
        assert_eq!(
            with_override("WINEDLLOVERRIDES=msimg32,winmm=b %command%"),
            "WINEDLLOVERRIDES=\"msimg32=n,b;winmm=b\" %command%"
        );
        assert_eq!(with_override(LAUNCH_LINE), LAUNCH_LINE);
        for l in ["", "-provider Portal", "WINEDLLOVERRIDES=\"d3d11=n,b\" %command%"] {
            assert!(has_override(&with_override(l)), "{l}");
        }
    }

    #[test]
    fn prefix_registry_override() {
        let reg = "WINE REGISTRY Version 2\n\n[Software\\\\Wine\\\\DllOverrides] 1700000000\n#time=1\n\"d3d11\"=\"native,builtin\"\n\"msimg32\"=\"native,builtin\"\n\n[Software\\\\Wine\\\\Other]\n";
        assert!(prefix_has_override(reg));
        assert!(!prefix_has_override(&reg.replace("\"msimg32\"=\"native,builtin\"", "\"msimg32\"=\"builtin\"")));
        assert!(!prefix_has_override("[Software\\\\Other]\n\"msimg32\"=\"native\"\n"));
    }

    fn steam_with_account(launch: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let cfg = root.path().join("userdata").join("123456").join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        let app = match launch {
            Some(l) => format!("\t\t\t\t\t\"1284210\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t{}\n\t\t\t\t\t}}\n", vdf::quote(l)),
            None => String::new(),
        };
        let text = format!(
            "\"UserLocalConfigStore\"\n{{\n\t\"Software\"\n\t{{\n\t\t\"Valve\"\n\t\t{{\n\t\t\t\"Steam\"\n\t\t\t{{\n\t\t\t\t\"apps\"\n\t\t\t\t{{\n{app}\t\t\t\t}}\n\t\t\t}}\n\t\t}}\n\t}}\n}}\n"
        );
        let file = cfg.join("localconfig.vdf");
        std::fs::write(&file, text).unwrap();
        // "0" is Steam's anonymous user, not an account.
        std::fs::create_dir_all(root.path().join("userdata").join("0").join("config")).unwrap();
        std::fs::write(root.path().join("userdata").join("0").join("config").join("localconfig.vdf"), "").unwrap();
        (root, file)
    }

    #[test]
    fn launch_options_are_read_and_written_per_account() {
        let (root, file) = steam_with_account(Some("-provider Portal"));
        assert_eq!(localconfig_files(root.path()), vec![file.clone()]);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(read_launch_options(&text).as_deref(), Some("-provider Portal"));
        let new = vdf::set(&text, &launch_options_path(), &with_override("-provider Portal")).unwrap();
        assert_eq!(read_launch_options(&new).unwrap(), format!("{LAUNCH_LINE} -provider Portal"));

        let (_root2, file2) = steam_with_account(None);
        let text = std::fs::read_to_string(&file2).unwrap();
        assert_eq!(read_launch_options(&text), None);
        let new = vdf::set(&text, &launch_options_path(), LAUNCH_LINE).unwrap();
        assert_eq!(read_launch_options(&new).as_deref(), Some(LAUNCH_LINE));
    }

    #[test]
    fn prefix_is_in_the_games_library() {
        let g = Path::new("/home/u/.local/share/Steam/steamapps/common/Guild Wars 2");
        assert_eq!(prefix_of(g), Some(PathBuf::from("/home/u/.local/share/Steam/steamapps/compatdata/1284210/pfx")));
        assert_eq!(prefix_of(Path::new("/opt/gw2")), None);
    }
}
