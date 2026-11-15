//! Finding the Guild Wars 2 installs on this computer (see docs/launch-methods.md).
//!
//! Windows:
//! - Steam (app 1284210): every library in `steamapps\libraryfolders.vdf` that has `appmanifest_1284210.acf`, plus
//!   Steam's own uninstall entry `Steam App 1284210`.
//! - Epic Games Store: the launcher's `*.item` manifests in `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests`.
//! - ArenaNet's own installer: `HKLM\SOFTWARE\ArenaNet\Guild Wars 2` (`Path`), the uninstall entries, and the
//!   usual folders.
//!
//! Linux:
//! - Steam (the game runs through Proton): native (`~/.steam/steam`, `~/.local/share/Steam`), Flatpak
//!   (`~/.var/app/com.valvesoftware.Steam/...`) and Snap (`~/snap/steam/common/...`), every library of each.
//! - ArenaNet's client in a Wine or Lutris prefix (`~/.wine`, `~/Games/<name>`), under `drive_c`.
//!
//! A folder counts only when it has `Gw2-64.exe`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::setup::is_game_dir;
use crate::vdf;

pub const STEAM_APP_ID: &str = "1284210";
const STEAM_APP_DIR: &str = "Guild Wars 2";
/// Part of a Flatpak Steam's paths.
pub const FLATPAK_STEAM: &str = "com.valvesoftware.Steam";

/// How an install is bought and started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    /// ArenaNet's own client (guildwars2.com): `Gw2-64.exe` is its own launcher and updater (on Linux, in Wine).
    Standalone,
    /// Steam: started through Steam so the Steam account logs in (on Linux, through Proton).
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
    /// The Steam installation it belongs to (Linux: for Proton's launch options and the Flatpak launch).
    #[serde(default)]
    pub steam_root: Option<PathBuf>,
    /// The Wine prefix it lives in (Linux, ArenaNet client under Wine or Lutris).
    #[serde(default)]
    pub wine_prefix: Option<PathBuf>,
}

impl GameInstall {
    pub fn new(path: PathBuf, channel: Channel, source: &str) -> Self {
        Self { path, channel, source: source.into(), epic: None, steam_root: None, wine_prefix: None }
    }

    /// Steam from Flatpak (launched with `flatpak run`).
    pub fn flatpak(&self) -> bool {
        self.steam_root.as_ref().is_some_and(|r| r.to_string_lossy().contains(FLATPAK_STEAM))
    }
}

/// Same folder: case-insensitive with either slash on Windows, exact (but for a trailing slash) elsewhere.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy();
        let s = s.trim_end_matches(['\\', '/']);
        if cfg!(windows) { s.replace('/', r"\").to_lowercase() } else { s.to_string() }
    };
    norm(a) == norm(b)
}

/// Steam library folders (from `steamapps/libraryfolders.vdf`), Steam's own folder first. Reads both the current
/// format (`"0" { "path" "..." }`) and the old one (`"1" "D:\\Games"`).
pub fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut out = vec![steam.to_path_buf()];
    let file = steam.join("steamapps").join("libraryfolders.vdf");
    if let Some(root) = std::fs::read_to_string(file).ok().and_then(|t| vdf::parse(&t).ok()) {
        let list = root.get("libraryfolders").or_else(|| root.get("LibraryFolders"));
        for (key, v) in list.map(|l| l.entries()).unwrap_or_default() {
            let p = match v {
                vdf::Value::Block { .. } => v.get("path").and_then(vdf::Value::str),
                vdf::Value::Str { text, .. } if key.chars().all(|c| c.is_ascii_digit()) => Some(text.as_str()),
                _ => None,
            };
            if let Some(p) = p.filter(|p| !p.is_empty()) {
                let p = PathBuf::from(p);
                if !out.iter().any(|o| same_path(o, &p)) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// The game folder in a Steam library, when that library has Guild Wars 2 installed (its app manifest).
pub fn steam_game_in(lib: &Path) -> Option<PathBuf> {
    let manifest = lib.join("steamapps").join(format!("appmanifest_{STEAM_APP_ID}.acf"));
    let text = std::fs::read_to_string(manifest).ok()?;
    let dir = vdf::parse(&text)
        .ok()
        .and_then(|v| v.path(&["AppState", "installdir"]).and_then(vdf::Value::str).map(str::to_string))
        .filter(|d| !d.is_empty() && !d.contains(['/', '\\']) && d != "..")
        .unwrap_or_else(|| STEAM_APP_DIR.into());
    Some(lib.join("steamapps").join("common").join(dir))
}

/// The Steam installs of the game under these Steam folders.
pub fn steam_installs(roots: &[PathBuf]) -> Vec<GameInstall> {
    let mut out: Vec<GameInstall> = Vec::new();
    for root in roots {
        for lib in steam_libraries(root) {
            // Without a manifest, the default folder still counts when the game is there.
            let dir = steam_game_in(&lib)
                .unwrap_or_else(|| lib.join("steamapps").join("common").join(STEAM_APP_DIR));
            if is_game_dir(&dir) && !out.iter().any(|g| same_path(&g.path, &dir)) {
                let mut g = GameInstall::new(dir, Channel::Steam, "Steam");
                g.steam_root = Some(root.clone());
                out.push(g);
            }
        }
    }
    out
}

/// The Steam library folder (`<library>`) of a game folder `<library>/steamapps/common/<dir>`.
pub fn library_of(game: &Path) -> Option<PathBuf> {
    let common = game.parent()?;
    let steamapps = common.parent()?;
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    (name(common).as_deref() == Some("common") && name(steamapps).as_deref() == Some("steamapps"))
        .then(|| steamapps.parent().map(Path::to_path_buf))
        .flatten()
}

/// The Wine prefix (the folder with `drive_c`) a game folder is in.
pub fn wine_prefix_of(game: &Path) -> Option<PathBuf> {
    game.ancestors().find(|a| a.file_name().is_some_and(|n| n == "drive_c")).and_then(|d| d.parent()).map(Path::to_path_buf)
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
pub fn parse_epic_item(text: &str) -> Option<(PathBuf, EpicApp)> {
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

/// A registry path value that may point at the exe or at the folder.
pub fn folder_of(p: &str) -> PathBuf {
    let p = PathBuf::from(p.trim().trim_matches('"'));
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        p.parent().map(Path::to_path_buf).unwrap_or(p)
    } else {
        p
    }
}

/// The program path of an uninstall entry value: `"C:\...\Gw2-64.exe" -uninstall`, `C:\...\Gw2-64.exe,0` or a folder.
pub fn exe_part(v: &str) -> &str {
    let v = v.trim().trim_start_matches('"');
    match v.to_ascii_lowercase().find(".exe") {
        Some(i) => &v[..i + 4],
        None => v.trim_end_matches('"'),
    }
}

/// Adds `g` unless its folder is already listed or has no game; a store install replaces a plain folder match.
fn push(found: &mut Vec<GameInstall>, g: GameInstall) {
    if !is_game_dir(&g.path) {
        return;
    }
    if let Some(existing) = found.iter_mut().find(|f| same_path(&f.path, &g.path)) {
        if existing.channel == Channel::Standalone && g.channel != Channel::Standalone {
            *existing = g;
        }
        return;
    }
    found.push(g);
}

/// Every Guild Wars 2 install found, each folder once.
pub fn find_installs() -> Vec<GameInstall> {
    let mut found = Vec::new();
    for g in platform::candidates() {
        push(&mut found, g);
    }
    // The installer extracted into the game folder.
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        push(&mut found, classify(&dir));
    }
    found
}

/// What a folder the player picked is: a store's when it is in one of its libraries, else the ArenaNet client.
pub fn classify(path: &Path) -> GameInstall {
    if let Some(g) = platform::candidates().into_iter().find(|g| same_path(&g.path, path)) {
        return g;
    }
    let mut g = GameInstall::new(path.to_path_buf(), Channel::Standalone, "Escolhida por você");
    if let Some(lib) = library_of(path) {
        // In a Steam library that is not registered (or Steam is not found): still Steam.
        g.channel = Channel::Steam;
        g.steam_root = platform::steam_roots().into_iter().find(|r| steam_libraries(r).iter().any(|l| same_path(l, &lib)));
    } else if !cfg!(windows) {
        g.wine_prefix = wine_prefix_of(path);
    }
    g
}

#[cfg(windows)]
mod platform {
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW,
        RegGetValueW, RegOpenKeyExW,
    };

    use super::*;

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

    pub fn steam_roots() -> Vec<PathBuf> {
        reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
            .or_else(|| reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"))
            .or_else(|| reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"))
            .map(|p| PathBuf::from(p.replace('/', r"\")))
            .into_iter()
            .collect()
    }

    /// `steam.exe`, to pass launch arguments with `-applaunch`.
    pub fn steam_exe() -> Option<PathBuf> {
        reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamExe")
            .map(|p| PathBuf::from(p.replace('/', r"\")))
            .or_else(|| steam_roots().first().map(|d| d.join("steam.exe")))
            .filter(|p| p.is_file())
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

    pub fn candidates() -> Vec<GameInstall> {
        let mut out = steam_installs(&steam_roots());
        let standalone = |p: PathBuf, source: &str| GameInstall::new(p, Channel::Standalone, source);
        let uninstall = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
        if let Some(p) =
            reg_string(HKEY_LOCAL_MACHINE, &format!(r"{uninstall}\Steam App {STEAM_APP_ID}"), "InstallLocation")
        {
            out.push(GameInstall { steam_root: steam_roots().first().cloned(), ..GameInstall::new(p.into(), Channel::Steam, "Steam") });
        }
        for (path, app) in epic_installs() {
            out.push(GameInstall { epic: Some(app), ..GameInstall::new(path, Channel::Epic, "Epic Games") });
        }
        for (root, key) in [
            (HKEY_LOCAL_MACHINE, r"SOFTWARE\ArenaNet\Guild Wars 2"),
            (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\ArenaNet\Guild Wars 2"),
            (HKEY_CURRENT_USER, r"Software\ArenaNet\Guild Wars 2"),
        ] {
            if let Some(p) = reg_string(root, key, "Path") {
                out.push(standalone(folder_of(&p), "Instalador da ArenaNet"));
            }
        }
        for (root, key) in [
            (HKEY_LOCAL_MACHINE, uninstall),
            (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
            (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
        ] {
            for sub in reg_subkeys(root, key) {
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
                        out.push(standalone(folder_of(exe_part(&v)), "Instalador da ArenaNet"));
                    }
                }
            }
        }
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Ok(p) = std::env::var(var) {
                out.push(standalone(Path::new(&p).join("Guild Wars 2"), "Pasta padrão"));
            }
        }
        for letter in b'C'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if !root.exists() {
                continue;
            }
            for sub in ["Guild Wars 2", r"Games\Guild Wars 2", r"Jogos\Guild Wars 2", r"Program Files\Guild Wars 2"] {
                out.push(standalone(root.join(sub), "Pasta padrão"));
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::{Path, PathBuf};

    use super::*;

    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    /// Steam folders under `home`: native, Flatpak and Snap (each once, whatever the symlinks).
    pub fn steam_roots_in(home: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for rel in [
            ".steam/steam",
            ".steam/root",
            ".local/share/Steam",
            ".var/app/com.valvesoftware.Steam/.local/share/Steam",
            ".var/app/com.valvesoftware.Steam/data/Steam",
            "snap/steam/common/.local/share/Steam",
            "snap/steam/common/.steam/steam",
        ] {
            let p = home.join(rel);
            if !p.join("steamapps").is_dir() {
                continue;
            }
            let real = std::fs::canonicalize(&p).unwrap_or(p);
            if !out.iter().any(|o| o == &real) {
                out.push(real);
            }
        }
        out
    }

    pub fn steam_roots() -> Vec<PathBuf> {
        home().map(|h| steam_roots_in(&h)).unwrap_or_default()
    }

    /// The ArenaNet client in Wine prefixes under `home`: `~/.wine` and Lutris' `~/Games/<name>`.
    pub fn wine_installs_in(home: &Path) -> Vec<GameInstall> {
        let mut prefixes = vec![home.join(".wine")];
        if let Ok(games) = std::fs::read_dir(home.join("Games")) {
            prefixes.extend(games.flatten().map(|e| e.path()));
        }
        let mut out = Vec::new();
        for prefix in prefixes {
            for pf in ["Program Files", "Program Files (x86)"] {
                let dir = prefix.join("drive_c").join(pf).join("Guild Wars 2");
                if is_game_dir(&dir) {
                    let mut g = GameInstall::new(dir, Channel::Standalone, "Wine / Lutris");
                    g.wine_prefix = Some(prefix.clone());
                    out.push(g);
                }
            }
        }
        out
    }

    pub fn candidates() -> Vec<GameInstall> {
        let Some(home) = home() else { return Vec::new() };
        let mut out = steam_installs(&steam_roots_in(&home));
        out.extend(wine_installs_in(&home));
        out
    }
}

pub use platform::steam_roots;
#[cfg(windows)]
pub use platform::steam_exe;
#[cfg(not(windows))]
pub use platform::{steam_roots_in, wine_installs_in};

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn parses_steam_library_paths() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("steamapps").join("libraryfolders.vdf"),
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"1284210\"\t\t\"80000000000\"\n\t\t}\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/mnt/games/SteamLibrary\"\n\t}\n}\n",
        );
        let libs = steam_libraries(dir.path());
        assert_eq!(libs[1], PathBuf::from(r"D:\SteamLibrary"));
        assert_eq!(libs[2], PathBuf::from("/mnt/games/SteamLibrary"));
        // The old format: numbered string values.
        write(
            &dir.path().join("steamapps").join("libraryfolders.vdf"),
            "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\t\"1\"\n\t\"1\"\t\t\"E:\\\\Games\"\n}\n",
        );
        assert_eq!(steam_libraries(dir.path()), vec![dir.path().to_path_buf(), PathBuf::from(r"E:\Games")]);
    }

    #[test]
    fn steam_manifest_gives_the_install_folder() {
        let lib = tempfile::tempdir().unwrap();
        assert_eq!(steam_game_in(lib.path()), None, "no manifest: not installed in this library");
        let manifest = lib.path().join("steamapps").join("appmanifest_1284210.acf");
        write(&manifest, "\"AppState\"\n{\n\t\"appid\"\t\t\"1284210\"\n\t\"installdir\"\t\t\"GW2 Custom\"\n}\n");
        assert_eq!(steam_game_in(lib.path()).unwrap(), lib.path().join("steamapps").join("common").join("GW2 Custom"));
        write(&manifest, "\"AppState\"\n{\n\t\"installdir\"\t\t\"../evil\"\n}\n");
        assert_eq!(steam_game_in(lib.path()).unwrap(), lib.path().join("steamapps").join("common").join("Guild Wars 2"));
    }

    #[test]
    fn steam_installs_are_found_in_every_library() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        write(
            &root.path().join("steamapps").join("libraryfolders.vdf"),
            &format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t{}\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t{}\n\t}}\n}}\n",
                vdf::quote(&root.path().to_string_lossy()),
                vdf::quote(&other.path().to_string_lossy())
            ),
        );
        write(&other.path().join("steamapps").join("appmanifest_1284210.acf"), "\"AppState\"\n{\n\t\"installdir\"\t\t\"Guild Wars 2\"\n}\n");
        let game = other.path().join("steamapps").join("common").join("Guild Wars 2");
        write(&game.join("Gw2-64.exe"), "");
        let found = steam_installs(&[root.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, game);
        assert_eq!(found[0].channel, Channel::Steam);
        assert_eq!(found[0].steam_root.as_deref(), Some(root.path()));
        assert_eq!(library_of(&game).as_deref(), Some(other.path()));
        assert_eq!(library_of(root.path()), None);
    }

    #[test]
    fn wine_prefix_is_the_folder_with_drive_c() {
        let g = Path::new("/home/u/Games/guild-wars-2/drive_c/Program Files/Guild Wars 2");
        assert_eq!(wine_prefix_of(g), Some(PathBuf::from("/home/u/Games/guild-wars-2")));
        assert_eq!(wine_prefix_of(Path::new("/opt/gw2")), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_steam_roots_and_wine_prefixes() {
        let home = tempfile::tempdir().unwrap();
        let flatpak = home.path().join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
        std::fs::create_dir_all(flatpak.join("steamapps")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/Steam/steamapps")).unwrap();
        // ~/.steam/steam is usually a symlink to ~/.local/share/Steam: listed once.
        std::fs::create_dir_all(home.path().join(".steam")).unwrap();
        std::os::unix::fs::symlink(home.path().join(".local/share/Steam"), home.path().join(".steam/steam")).unwrap();
        let roots = steam_roots_in(home.path());
        assert_eq!(roots.len(), 2, "{roots:?}");
        assert!(roots.iter().any(|r| r.to_string_lossy().contains(FLATPAK_STEAM)));

        let lutris = home.path().join("Games/guild-wars-2/drive_c/Program Files/Guild Wars 2");
        write(&lutris.join("Gw2-64.exe"), "");
        let wine = wine_installs_in(home.path());
        assert_eq!(wine.len(), 1);
        assert_eq!(wine[0].wine_prefix.as_deref(), Some(home.path().join("Games/guild-wars-2").as_path()));
    }

    #[test]
    fn epic_manifest_of_guild_wars_2_is_recognised() {
        let item = r#"{"DisplayName":"Guild Wars 2","InstallLocation":"E:\\Epic Games\\GuildWars2",
            "LaunchExecutable":"Gw2-64.exe","AppName":"a1","CatalogNamespace":"ns","CatalogItemId":"cat",
            "MainGameAppName":"Main","MainGameCatalogNamespace":"mns","MainGameCatalogItemId":"mcat",
            "bIsIncompleteInstall":false}"#;
        let (path, app) = parse_epic_item(item).unwrap();
        assert_eq!(path, PathBuf::from(r"E:\Epic Games\GuildWars2"));
        assert_eq!(app, EpicApp { namespace: "mns".into(), catalog_item_id: "mcat".into(), app_name: "Main".into() });
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
        assert_eq!(exe_part(r#""C:\Guild Wars 2\Gw2-64.exe" -uninstall"#), r"C:\Guild Wars 2\Gw2-64.exe");
        assert_eq!(exe_part(r"C:\Guild Wars 2\Gw2-64.exe,0"), r"C:\Guild Wars 2\Gw2-64.exe");
        assert_eq!(exe_part(r"C:\Guild Wars 2"), r"C:\Guild Wars 2");
        assert_eq!(folder_of("/opt/gw2"), PathBuf::from("/opt/gw2"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_without_case() {
        assert_eq!(folder_of(r"C:\Program Files\Guild Wars 2\Gw2-64.exe"), PathBuf::from(r"C:\Program Files\Guild Wars 2"));
        assert_eq!(folder_of(r#""D:\GW2\Gw2-64.exe""#), PathBuf::from(r"D:\GW2"));
        assert!(same_path(Path::new(r"c:/program files/guild wars 2/"), Path::new(r"C:\Program Files\Guild Wars 2")));
    }
}
