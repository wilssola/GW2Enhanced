//! Starting the game the way its store expects (see docs/launch-methods.md).
//!
//! Windows:
//! - Standalone (ArenaNet): `Gw2-64.exe` from its folder, with the chosen arguments; it patches itself first.
//! - Steam: `steam://rungameid/1284210`, so Steam logs the Steam account in (the game refuses to start a Steam
//!   account without Steam). With arguments, `steam.exe -applaunch 1284210 <args>`, which Steam passes on.
//! - Epic: `com.epicgames.launcher://apps/<namespace>%3A<item>%3A<app>?action=launch&silent=true`. The Epic launcher
//!   takes no arguments from outside; they go in Epic's own "Launch Options".
//!
//! Linux:
//! - Steam: `steam steam://rungameid/1284210` (`flatpak run com.valvesoftware.Steam ...` for the Flatpak, else
//!   `xdg-open`), so Steam runs it through Proton; with arguments, `steam -applaunch 1284210 <args>`.
//! - Standalone in Wine/Lutris: `wine Gw2-64.exe` with `WINEPREFIX` and `WINEDLLOVERRIDES=msimg32=n,b`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::games::{Channel, GameInstall, STEAM_APP_ID};
use crate::setup::GAME_EXE;

/// The launch arguments the player can pick (all off by default).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchOptions {
    /// `-autologin`: logs in with the remembered account and password.
    pub autologin: bool,
    /// `-shareArchive`: opens Gw2.dat shared (several clients, tools that read it).
    pub share_archive: bool,
    /// `-mapLoadinfo`: load progress and time on loading screens.
    pub map_load_info: bool,
    /// `-windowed`.
    pub windowed: bool,
    /// Anything else, as typed (quotes group words).
    pub extra: String,
}

impl LaunchOptions {
    pub fn args(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (on, arg) in [
            (self.autologin, "-autologin"),
            (self.share_archive, "-shareArchive"),
            (self.map_load_info, "-mapLoadinfo"),
            (self.windowed, "-windowed"),
        ] {
            if on {
                out.push(arg.to_string());
            }
        }
        out.extend(split_args(&self.extra));
        out
    }
}

/// Splits a command line the simple way: spaces separate, double quotes group.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// What starting an install runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// A program, from a folder, with extra environment variables.
    Program { exe: PathBuf, args: Vec<String>, cwd: Option<PathBuf>, env: Vec<(String, String)> },
    /// A link handed to the program registered for it (Steam, Epic).
    Url(String),
}

/// What the computer has to start the game with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tools {
    /// Linux rules (Proton, Wine) instead of Windows ones.
    pub linux: bool,
    /// The command that runs Steam (`steam.exe`; `steam`; `flatpak run com.valvesoftware.Steam`).
    pub steam: Option<Vec<String>>,
    /// Wine (Linux, ArenaNet client).
    pub wine: Option<PathBuf>,
}

impl Tools {
    /// What this computer has, for `game`.
    pub fn find(game: &GameInstall) -> Self {
        #[cfg(windows)]
        {
            let _ = game;
            Tools { linux: false, steam: crate::games::steam_exe().map(|p| vec![p.display().to_string()]), wine: None }
        }
        #[cfg(not(windows))]
        {
            let steam = if game.flatpak() {
                which("flatpak").map(|f| vec![f.display().to_string(), "run".into(), crate::games::FLATPAK_STEAM.into()])
            } else {
                which("steam").map(|s| vec![s.display().to_string()])
            };
            Tools { linux: true, steam, wine: which("wine") }
        }
    }
}

/// A program in `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

fn program(exe: PathBuf, args: Vec<String>) -> Plan {
    Plan::Program { exe, args, cwd: None, env: Vec::new() }
}

/// How to start `game`. `patch_only` (`-image`): the ArenaNet client downloads the update and closes.
pub fn plan(game: &GameInstall, opts: &LaunchOptions, patch_only: bool, tools: &Tools) -> Result<Plan, String> {
    let mut args = opts.args();
    match game.channel {
        Channel::Standalone => {
            if patch_only {
                args = vec!["-image".into()];
            }
            let exe = game.path.join(GAME_EXE);
            if !tools.linux {
                return Ok(Plan::Program { exe, args, cwd: Some(game.path.clone()), env: Vec::new() });
            }
            let wine = tools.wine.clone().ok_or_else(|| {
                tr!(
                    "o Wine não foi encontrado; abra o jogo pelo Lutris (com o override msimg32=n,b nas opções do Wine) \
                     ou instale o Wine",
                    "Wine was not found; start the game through Lutris (with the msimg32=n,b override in the Wine options) \
                     or install Wine"
                )
            })?;
            let mut env = vec![("WINEDLLOVERRIDES".to_string(), crate::proton::DLL_OVERRIDE.to_string())];
            if let Some(prefix) = &game.wine_prefix {
                env.push(("WINEPREFIX".into(), prefix.display().to_string()));
            }
            let mut a = vec![exe.display().to_string()];
            a.extend(args);
            Ok(Plan::Program { exe: wine, args: a, cwd: Some(game.path.clone()), env })
        }
        Channel::Steam => {
            if patch_only {
                return Err(tr!("na Steam, o jogo é atualizado pela própria Steam", "on Steam, the game is updated by Steam itself"));
            }
            let url = format!("steam://rungameid/{STEAM_APP_ID}");
            match (&tools.steam, args.is_empty()) {
                (None, true) => Ok(Plan::Url(url)),
                (Some(cmd), true) if tools.linux => {
                    let mut a = cmd[1..].to_vec();
                    a.push(url);
                    Ok(program(PathBuf::from(&cmd[0]), a))
                }
                (Some(_), true) => Ok(Plan::Url(url)),
                (Some(cmd), false) => {
                    let mut a = cmd[1..].to_vec();
                    a.extend(["-applaunch".to_string(), STEAM_APP_ID.to_string()]);
                    a.extend(args);
                    Ok(program(PathBuf::from(&cmd[0]), a))
                }
                (None, false) => Ok(Plan::Url(format!(
                    "steam://run/{STEAM_APP_ID}//{}/",
                    args.iter().map(|a| a.replace(' ', "%20")).collect::<Vec<_>>().join("%20")
                ))),
            }
        }
        Channel::Epic => {
            if tools.linux {
                return Err(tr!(
                    "no Linux, a versão da Epic Games roda pelo Heroic Games Launcher: abra o jogo por ele",
                    "on Linux, the Epic Games version runs through Heroic Games Launcher: start the game from it"
                ));
            }
            if patch_only {
                return Err(tr!("na Epic Games, o jogo é atualizado pelo próprio launcher da Epic", "on Epic Games, the game is updated by the Epic launcher itself"));
            }
            let app = game
                .epic
                .as_ref()
                .ok_or_else(|| tr!("não encontrei os dados da Epic Games desta instalação", "could not find the Epic Games data of this install"))?;
            let part = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !(part(&app.namespace) && part(&app.catalog_item_id) && part(&app.app_name)) || app.app_name.is_empty() {
                return Err(tr!("os dados da Epic Games desta instalação são inválidos", "the Epic Games data of this install is invalid"));
            }
            let id = if app.namespace.is_empty() || app.catalog_item_id.is_empty() {
                app.app_name.clone()
            } else {
                format!("{}%3A{}%3A{}", app.namespace, app.catalog_item_id, app.app_name)
            };
            Ok(Plan::Url(format!("com.epicgames.launcher://apps/{id}?action=launch&silent=true")))
        }
    }
}

/// Runs a plan without waiting for it.
pub fn run(plan: &Plan) -> Result<(), String> {
    match plan {
        Plan::Program { exe, args, cwd, env } => {
            if !exe.is_file() {
                return Err(tr!("{} não existe", "{} does not exist", exe.display()));
            }
            let mut cmd = Command::new(exe);
            cmd.args(args).envs(env.iter().map(|(k, v)| (k, v)));
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            cmd.spawn().map(|_| ()).map_err(|e| tr!("não consegui abrir {}: {e}", "could not open {}: {e}", exe.display()))
        }
        Plan::Url(url) => shell_open(url),
    }
}

/// Hands a link or folder to the system (`ShellExecuteW` "open" / `xdg-open`).
#[cfg(windows)]
pub fn shell_open(target: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (verb, file) = (w("open"), w(target));
    let h = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // ShellExecute returns a value above 32 on success.
    if h as usize > 32 { Ok(()) } else { Err(tr!("o Windows não conseguiu abrir {target} (código {})", "Windows could not open {target} (code {})", h as usize)) }
}

#[cfg(not(windows))]
pub fn shell_open(target: &str) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| tr!("não consegui abrir {target} (xdg-open: {e})", "could not open {target} (xdg-open: {e})"))
}

/// Every running `Gw2-64.exe`: its full path, or `None` when the system does not tell.
#[cfg(windows)]
pub fn running_games() -> Vec<Option<PathBuf>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(GAME_EXE) {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.th32ProcessID);
                let mut path = None;
                if !h.is_null() {
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0 {
                        path = Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])));
                    }
                    CloseHandle(h);
                }
                out.push(path);
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

/// Under Wine the game's command line names `Gw2-64.exe` as a Windows path (`Z:\home\...`) or a Unix one.
#[cfg(not(windows))]
pub fn running_games() -> Vec<Option<PathBuf>> {
    let mut out = Vec::new();
    let Ok(procs) = std::fs::read_dir("/proc") else { return out };
    for p in procs.flatten() {
        if !p.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(p.path().join("cmdline")) else { continue };
        let args: Vec<String> = cmdline.split(|&b| b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        // Only the process itself (its first argument), not the launchers that mention it further on.
        if let Some(first) = args.first()
            && first.to_ascii_lowercase().replace('\\', "/").ends_with("gw2-64.exe")
        {
            out.push(unix_path_of(first));
        }
    }
    out
}

/// A Wine path of the game (`Z:\home\u\...\Gw2-64.exe`, or a Unix path) as a Unix path, when it says.
pub fn unix_path_of(arg: &str) -> Option<PathBuf> {
    let a = arg.replace('\\', "/");
    if a.starts_with('/') {
        Some(PathBuf::from(a))
    } else if a.len() > 2 && a[..2].eq_ignore_ascii_case("z:") {
        Some(PathBuf::from(&a[2..]))
    } else {
        None
    }
}

/// Steam is running (Linux: it rewrites its config files while it runs).
#[cfg(not(windows))]
pub fn steam_running() -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else { return false };
    procs.flatten().any(|p| {
        std::fs::read_to_string(p.path().join("comm")).is_ok_and(|c| matches!(c.trim(), "steam" | "steamwebhelper"))
    })
}

#[cfg(windows)]
pub fn steam_running() -> bool {
    false
}

/// The `Gw2-64.exe` of `dir` is running (a copy whose path is unknown counts too: the safe side).
pub fn game_running_in(dir: &Path) -> bool {
    running_in(&running_games(), dir)
}

pub fn running_in(running: &[Option<PathBuf>], dir: &Path) -> bool {
    running.iter().any(|p| match p {
        Some(exe) => exe.parent().is_some_and(|d| crate::games::same_path(d, dir)),
        None => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::{EpicApp, FLATPAK_STEAM};

    fn install(channel: Channel) -> GameInstall {
        GameInstall::new(PathBuf::from("/games/GW2"), channel, "")
    }

    fn win() -> Tools {
        Tools { linux: false, steam: Some(vec![r"C:\Steam\steam.exe".into()]), wine: None }
    }

    #[test]
    fn arguments_from_options_and_free_text() {
        let o = LaunchOptions {
            autologin: true,
            map_load_info: true,
            extra: r#"-clientport 443 -dat "D:\My Files\Gw2.dat""#.into(),
            ..Default::default()
        };
        assert_eq!(o.args(), ["-autologin", "-mapLoadinfo", "-clientport", "443", "-dat", r"D:\My Files\Gw2.dat"]);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args(r#"a "" b"#), ["a", "", "b"]);
    }

    #[test]
    fn standalone_runs_the_exe_from_its_folder() {
        let o = LaunchOptions { share_archive: true, ..Default::default() };
        let g = install(Channel::Standalone);
        assert_eq!(
            plan(&g, &o, false, &win()).unwrap(),
            Plan::Program { exe: g.path.join("Gw2-64.exe"), args: vec!["-shareArchive".into()], cwd: Some(g.path.clone()), env: vec![] }
        );
        let Plan::Program { args, .. } = plan(&g, &o, true, &win()).unwrap() else { panic!() };
        assert_eq!(args, ["-image"]);
    }

    #[test]
    fn standalone_on_linux_runs_in_wine_with_the_override() {
        let mut g = install(Channel::Standalone);
        g.wine_prefix = Some(PathBuf::from("/home/u/Games/gw2"));
        let tools = Tools { linux: true, steam: None, wine: Some(PathBuf::from("/usr/bin/wine")) };
        let Plan::Program { exe, args, env, .. } = plan(&g, &LaunchOptions::default(), false, &tools).unwrap() else { panic!() };
        assert_eq!(exe, PathBuf::from("/usr/bin/wine"));
        assert_eq!(args, vec![g.path.join("Gw2-64.exe").display().to_string()]);
        assert!(env.contains(&("WINEDLLOVERRIDES".into(), "msimg32=n,b".into())));
        assert!(env.contains(&("WINEPREFIX".into(), "/home/u/Games/gw2".into())));
        assert!(plan(&g, &LaunchOptions::default(), false, &Tools { wine: None, ..tools }).is_err());
    }

    #[test]
    fn steam_goes_through_steam() {
        let none = LaunchOptions::default();
        let g = install(Channel::Steam);
        assert_eq!(plan(&g, &none, false, &win()).unwrap(), Plan::Url("steam://rungameid/1284210".into()));
        let o = LaunchOptions { autologin: true, ..Default::default() };
        assert_eq!(
            plan(&g, &o, false, &win()).unwrap(),
            program(PathBuf::from(r"C:\Steam\steam.exe"), vec!["-applaunch".into(), "1284210".into(), "-autologin".into()])
        );
        assert_eq!(plan(&g, &o, false, &Tools::default()).unwrap(), Plan::Url("steam://run/1284210//-autologin/".into()));
        assert!(plan(&g, &none, true, &win()).is_err());
    }

    #[test]
    fn steam_on_linux_runs_proton_through_the_steam_client() {
        let g = install(Channel::Steam);
        let none = LaunchOptions::default();
        let native = Tools { linux: true, steam: Some(vec!["/usr/bin/steam".into()]), wine: None };
        assert_eq!(
            plan(&g, &none, false, &native).unwrap(),
            program(PathBuf::from("/usr/bin/steam"), vec!["steam://rungameid/1284210".into()])
        );
        let flatpak = Tools {
            linux: true,
            steam: Some(vec!["/usr/bin/flatpak".into(), "run".into(), FLATPAK_STEAM.into()]),
            wine: None,
        };
        let o = LaunchOptions { windowed: true, ..Default::default() };
        assert_eq!(
            plan(&g, &o, false, &flatpak).unwrap(),
            program(
                PathBuf::from("/usr/bin/flatpak"),
                vec!["run".into(), FLATPAK_STEAM.into(), "-applaunch".into(), "1284210".into(), "-windowed".into()]
            )
        );
        // No steam command: xdg-open hands the link to whatever Steam is registered.
        let bare = Tools { linux: true, ..Default::default() };
        assert_eq!(plan(&g, &none, false, &bare).unwrap(), Plan::Url("steam://rungameid/1284210".into()));
        assert!(plan(&install(Channel::Epic), &none, false, &bare).is_err());
    }

    #[test]
    fn only_the_game_in_that_folder_blocks_it() {
        let dir = Path::new("/games/GW2");
        assert!(!running_in(&[], dir));
        assert!(running_in(&[Some(PathBuf::from("/games/GW2/Gw2-64.exe"))], dir));
        assert!(!running_in(&[Some(PathBuf::from("/other/Guild Wars 2/Gw2-64.exe"))], dir));
        assert!(running_in(&[None], dir), "a copy whose path is unknown may be this one");
        // This test process is not the game.
        assert!(!running_games().iter().flatten().any(|p| p == &std::env::current_exe().unwrap()));
        assert_eq!(unix_path_of(r"Z:\home\u\GW2\Gw2-64.exe"), Some(PathBuf::from("/home/u/GW2/Gw2-64.exe")));
        assert_eq!(unix_path_of(r"C:\Program Files\Guild Wars 2\Gw2-64.exe"), None);
    }

    #[test]
    fn epic_goes_through_the_epic_launcher() {
        let mut g = install(Channel::Epic);
        assert!(plan(&g, &LaunchOptions::default(), false, &win()).is_err());
        g.epic = Some(EpicApp { namespace: "ns1".into(), catalog_item_id: "abc".into(), app_name: "Gw2".into() });
        assert_eq!(
            plan(&g, &LaunchOptions::default(), false, &win()).unwrap(),
            Plan::Url("com.epicgames.launcher://apps/ns1%3Aabc%3AGw2?action=launch&silent=true".into())
        );
        g.epic = Some(EpicApp { namespace: String::new(), catalog_item_id: String::new(), app_name: "Gw2".into() });
        assert_eq!(
            plan(&g, &LaunchOptions::default(), false, &win()).unwrap(),
            Plan::Url("com.epicgames.launcher://apps/Gw2?action=launch&silent=true".into())
        );
        g.epic = Some(EpicApp { namespace: "a&b".into(), catalog_item_id: "x".into(), app_name: "y".into() });
        assert!(plan(&g, &LaunchOptions::default(), false, &win()).is_err());
    }
}
