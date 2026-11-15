//! Finding the Guild Wars 2 installs on this computer (see docs/launch-methods.md).
//!
//! - Steam (app 1284210): every library in `steamapps\libraryfolders.vdf` that has `appmanifest_1284210.acf`, plus
//!   Steam's own uninstall entry `Steam App 1284210`.
//! - Epic Games Store: the launcher's `*.item` manifests in `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests`.
//! - ArenaNet's own installer: `HKLM\SOFTWARE\ArenaNet\Guild Wars 2` (`Path`), the uninstall entries, and the
//!   usual folders.
//!
//! A folder counts only when it has `Gw2-64.exe`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW,
};

use crate::setup::is_game_dir;

pub const STEAM_APP_ID: &str = "1284210";
const STEAM_APP_DIR: &str = "Guild Wars 2";

/// How an install is bought and started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    /// ArenaNet's own client (guildwars2.com): `Gw2-64.exe` is its own launcher and updater.
    Standalone,
    /// Steam: started through Steam so the Steam account logs in.
    Steam,
    /// Epic Games Store: started through the Epic Games Launcher.
    Epic,
}

/// The Epic Games Launcher's ids of an install, for its `com.epicgames.launcher://apps/...` link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpicApp {
    pub namespace: String,
    pub catalog_item_id: String,
    pub app_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameInstall {
    pub path: PathBuf,
    pub channel: Channel,
    /// Where it was found (shown to the player).
    pub source: String,
    pub epic: Option<EpicApp>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

fn reg_subkeys(root: HKEY, key: &str) -> Vec<String> {
    let k = wide(key);
    let mut h: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(root, k.as_ptr(), 0, KEY_READ, &mut h) } != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                h,
                i,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    unsafe { RegCloseKey(h) };
    out
}

/// The Steam folder (registry), with forward slashes fixed.
pub fn steam_dir() -> Option<PathBuf> {
    reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        .or_else(|| reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"))
        .or_else(|| reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"))
        .map(|p| PathBuf::from(p.replace('/', r"\")))
}

/// `steam.exe`, to pass launch arguments with `-applaunch`.
pub fn steam_exe() -> Option<PathBuf> {
    reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamExe")
        .map(|p| PathBuf::from(p.replace('/', r"\")))
        .or_else(|| steam_dir().map(|d| d.join("steam.exe")))
        .filter(|p| p.is_file())
}

/// Steam library `path` folders (from `libraryfolders.vdf`), Steam's own folder first.
pub fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut out = vec![steam.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(steam.join(r"steamapps\libraryfolders.vdf")) {
        for line in vdf.lines() {
            let mut parts = line.split('"').filter(|p| !p.trim().is_empty());
            if parts.next() == Some("path")
                && let Some(p) = parts.next()
            {
                let p = PathBuf::from(p.replace(r"\\", r"\"));
                if !out.iter().any(|o| same_path(o, &p)) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// A `"key"  "value"` from a Valve KeyValues file (first match).
fn vdf_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split('"').filter(|p| !p.trim().is_empty());
        (parts.next()?.eq_ignore_ascii_case(key)).then(|| parts.next().map(|v| v.replace(r"\\", r"\")))?
    })
}

/// The game folder in a Steam library, when that library has Guild Wars 2 installed.
pub fn steam_game_in(lib: &Path) -> Option<PathBuf> {
    let manifest = std::fs::read_to_string(lib.join(format!(r"steamapps\appmanifest_{STEAM_APP_ID}.acf"))).ok();
    let dir = manifest.as_deref().and_then(|m| vdf_value(m, "installdir")).unwrap_or_else(|| STEAM_APP_DIR.into());
    Some(lib.join("steamapps").join("common").join(dir))
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).replace('/', r"\").to_lowercase();
    norm(a) == norm(b)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EpicItem {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    install_location: String,
    #[serde(default)]
    launch_executable: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    catalog_namespace: String,
    #[serde(default)]
    catalog_item_id: String,
    #[serde(default)]
    main_game_app_name: String,
    #[serde(default)]
    main_game_catalog_namespace: String,
    #[serde(default)]
    main_game_catalog_item_id: String,
    #[serde(default, rename = "bIsIncompleteInstall")]
    incomplete: bool,
}

/// A Guild Wars 2 install from an Epic Games Launcher manifest (`*.item`, JSON).
fn parse_epic_item(text: &str) -> Option<(PathBuf, EpicApp)> {
    let item: EpicItem = serde_json::from_str(text).ok()?;
    let exe = item.launch_executable.replace('/', r"\");
    let is_gw2 = exe.rsplit('\\').next().is_some_and(|e| e.eq_ignore_ascii_case("Gw2-64.exe"))
        || item.display_name.to_lowercase().starts_with("guild wars 2");
    if !is_gw2 || item.incomplete || item.install_location.is_empty() {
        return None;
    }
    let pick = |main: String, own: String| if main.is_empty() { own } else { main };
    let app = EpicApp {
        namespace: pick(item.main_game_catalog_namespace, item.catalog_namespace),
        catalog_item_id: pick(item.main_game_catalog_item_id, item.catalog_item_id),
        app_name: pick(item.main_game_app_name, item.app_name),
    };
    Some((PathBuf::from(item.install_location), app))
}

fn epic_installs() -> Vec<(PathBuf, EpicApp)> {
    let data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let dir = Path::new(&data).join(r"Epic\EpicGamesLauncher\Data\Manifests");
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")))
        .filter_map(|e| parse_epic_item(&std::fs::read_to_string(e.path()).ok()?))
        .collect()
}

/// A registry path value that may point at the exe or at the folder.
fn folder_of(p: &str) -> PathBuf {
    let p = PathBuf::from(p.trim().trim_matches('"'));
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        p.parent().map(Path::to_path_buf).unwrap_or(p)
    } else {
        p
    }
}

/// The program path of an uninstall entry value: `"C:\...\Gw2-64.exe" -uninstall`, `C:\...\Gw2-64.exe,0` or a folder.
fn exe_part(v: &str) -> &str {
    let v = v.trim().trim_start_matches('"');
    match v.to_ascii_lowercase().find(".exe") {
        Some(i) => &v[..i + 4],
        None => v.trim_end_matches('"'),
    }
}

/// Every Guild Wars 2 install found, each folder once (Steam and Epic win over a plain folder match).
pub fn find_installs() -> Vec<GameInstall> {
    let mut found: Vec<GameInstall> = Vec::new();
    let mut add = |path: PathBuf, channel: Channel, source: &str, epic: Option<EpicApp>| {
        if !is_game_dir(&path) {
            return;
        }
        if let Some(existing) = found.iter_mut().find(|f| same_path(&f.path, &path)) {
            if existing.channel == Channel::Standalone && channel != Channel::Standalone {
                *existing = GameInstall { path, channel, source: source.into(), epic };
            }
            return;
        }
        found.push(GameInstall { path, channel, source: source.into(), epic });
    };

    if let Some(steam) = steam_dir() {
        for lib in steam_libraries(&steam) {
            if lib.join(format!(r"steamapps\appmanifest_{STEAM_APP_ID}.acf")).is_file()
                && let Some(dir) = steam_game_in(&lib)
            {
                add(dir, Channel::Steam, "Steam", None);
            } else {
                add(lib.join(r"steamapps\common").join(STEAM_APP_DIR), Channel::Steam, "Steam", None);
            }
        }
    }
    let uninstall = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    if let Some(p) = reg_string(HKEY_LOCAL_MACHINE, &format!(r"{uninstall}\Steam App {STEAM_APP_ID}"), "InstallLocation")
    {
        add(PathBuf::from(p), Channel::Steam, "Steam", None);
    }
    for (path, app) in epic_installs() {
        add(path, Channel::Epic, "Epic Games", Some(app));
    }
    for (root, key) in [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\ArenaNet\Guild Wars 2"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\ArenaNet\Guild Wars 2"),
        (HKEY_CURRENT_USER, r"Software\ArenaNet\Guild Wars 2"),
    ] {
        if let Some(p) = reg_string(root, key, "Path") {
            add(folder_of(&p), Channel::Standalone, "Instalador da ArenaNet", None);
        }
    }
    for (root, key) in [
        (HKEY_LOCAL_MACHINE, uninstall.to_string()),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall".to_string()),
        (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall".to_string()),
    ] {
        for sub in reg_subkeys(root, &key) {
            if sub.starts_with("Steam App ") {
                continue;
            }
            let k = format!(r"{key}\{sub}");
            let name = reg_string(root, &k, "DisplayName").unwrap_or_default();
            if !name.to_lowercase().starts_with("guild wars 2") {
                continue;
            }
            for value in ["InstallLocation", "DisplayIcon", "UninstallString"] {
                if let Some(v) = reg_string(root, &k, value) {
                    add(folder_of(exe_part(&v)), Channel::Standalone, "Instalador da ArenaNet", None);
                }
            }
        }
    }
    let mut common = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(p) = std::env::var(var) {
            common.push(Path::new(&p).join("Guild Wars 2"));
        }
    }
    for letter in b'C'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\", letter as char));
        if !root.exists() {
            continue;
        }
        for sub in ["Guild Wars 2", r"Games\Guild Wars 2", r"Jogos\Guild Wars 2", r"Program Files\Guild Wars 2"] {
            common.push(root.join(sub));
        }
    }
    for p in common {
        add(p, Channel::Standalone, "Pasta padrão", None);
    }
    // The installer extracted into the game folder.
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        add(dir.clone(), Channel::Standalone, "Pasta do instalador", None);
    }
    found
}

/// What a folder the player picked is: Steam or Epic when it is one of theirs, else the standalone client.
pub fn classify(path: &Path) -> GameInstall {
    if let Some(steam) = steam_dir() {
        for lib in steam_libraries(&steam) {
            if path.starts_with(lib.join("steamapps")) || steam_game_in(&lib).is_some_and(|d| same_path(&d, path)) {
                return GameInstall { path: path.into(), channel: Channel::Steam, source: "Steam".into(), epic: None };
            }
        }
    }
    for (p, app) in epic_installs() {
        if same_path(&p, path) {
            return GameInstall {
                path: path.into(),
                channel: Channel::Epic,
                source: "Epic Games".into(),
                epic: Some(app),
            };
        }
    }
    // Not in a known library, but laid out like one.
    let s = path.to_string_lossy().to_lowercase();
    let channel = if s.contains(r"\steamapps\common\") { Channel::Steam } else { Channel::Standalone };
    GameInstall { path: path.into(), channel, source: "Escolhida por você".into(), epic: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steam_library_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steamapps")).unwrap();
        std::fs::write(
            dir.path().join(r"steamapps\libraryfolders.vdf"),
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n",
        )
        .unwrap();
        let libs = steam_libraries(dir.path());
        assert_eq!(libs[1], PathBuf::from(r"D:\SteamLibrary"));
    }

    #[test]
    fn steam_manifest_gives_the_install_folder() {
        let lib = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lib.path().join("steamapps")).unwrap();
        assert_eq!(steam_game_in(lib.path()).unwrap(), lib.path().join(r"steamapps\common\Guild Wars 2"));
        std::fs::write(
            lib.path().join(r"steamapps\appmanifest_1284210.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"1284210\"\n\t\"installdir\"\t\t\"GW2 Custom\"\n}\n",
        )
        .unwrap();
        assert_eq!(steam_game_in(lib.path()).unwrap(), lib.path().join(r"steamapps\common\GW2 Custom"));
    }

    #[test]
    fn epic_manifest_of_guild_wars_2_is_recognised() {
        let item = r#"{"DisplayName":"Guild Wars 2","InstallLocation":"E:\\Epic Games\\GuildWars2",
            "LaunchExecutable":"Gw2-64.exe","AppName":"a1","CatalogNamespace":"ns","CatalogItemId":"cat",
            "MainGameAppName":"Main","MainGameCatalogNamespace":"mns","MainGameCatalogItemId":"mcat",
            "bIsIncompleteInstall":false}"#;
        let (path, app) = parse_epic_item(item).unwrap();
        assert_eq!(path, PathBuf::from(r"E:\Epic Games\GuildWars2"));
        assert_eq!(
            app,
            EpicApp { namespace: "mns".into(), catalog_item_id: "mcat".into(), app_name: "Main".into() }
        );
        let own = r#"{"DisplayName":"Guild Wars 2","InstallLocation":"E:\\GW2","LaunchExecutable":"Gw2-64.exe",
            "AppName":"a1","CatalogNamespace":"ns","CatalogItemId":"cat"}"#;
        assert_eq!(parse_epic_item(own).unwrap().1.app_name, "a1");
        let other = r#"{"DisplayName":"Fortnite","InstallLocation":"F:\\Fortnite","LaunchExecutable":"x.exe"}"#;
        assert!(parse_epic_item(other).is_none());
        let partial = r#"{"DisplayName":"Guild Wars 2","InstallLocation":"E:\\GW2","bIsIncompleteInstall":true}"#;
        assert!(parse_epic_item(partial).is_none());
        assert!(parse_epic_item("not json").is_none());
    }

    #[test]
    fn registry_paths_may_name_the_exe_or_the_folder() {
        assert_eq!(folder_of(r"C:\Program Files\Guild Wars 2\Gw2-64.exe"), PathBuf::from(r"C:\Program Files\Guild Wars 2"));
        assert_eq!(folder_of(r#""D:\GW2\Gw2-64.exe""#), PathBuf::from(r"D:\GW2"));
        assert_eq!(folder_of(r"D:\GW2"), PathBuf::from(r"D:\GW2"));
        assert_eq!(exe_part(r#""C:\Guild Wars 2\Gw2-64.exe" -uninstall"#), r"C:\Guild Wars 2\Gw2-64.exe");
        assert_eq!(exe_part(r"C:\Guild Wars 2\Gw2-64.exe,0"), r"C:\Guild Wars 2\Gw2-64.exe");
        assert_eq!(exe_part(r"C:\Guild Wars 2"), r"C:\Guild Wars 2");
        assert!(same_path(Path::new(r"c:/program files/guild wars 2/"), Path::new(r"C:\Program Files\Guild Wars 2")));
    }
}
