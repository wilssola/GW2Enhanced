//! Starting the game the way its store expects (see docs/launch-methods.md).
//!
//! - Standalone (ArenaNet): `Gw2-64.exe` from its folder, with the chosen arguments; it patches itself first.
//! - Steam: `steam://rungameid/1284210`, so Steam logs the Steam account in (the game refuses to start a Steam
//!   account without Steam). With arguments, `steam.exe -applaunch 1284210 <args>`, which Steam passes on.
//! - Epic: `com.epicgames.launcher://apps/<namespace>%3A<item>%3A<app>?action=launch&silent=true`. The Epic launcher
//!   takes no arguments from outside; they go in Epic's own "Launch Options".

use std::path::PathBuf;
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
    /// A program, from a folder.
    Program { exe: PathBuf, args: Vec<String>, cwd: Option<PathBuf> },
    /// A link Windows hands to its registered program (Steam, Epic).
    Url(String),
}

/// How to start `game`. `patch_only` (`-image`): the standalone client downloads the update and closes.
/// `steam_exe` is Steam's program, used when there are arguments to pass.
pub fn plan(game: &GameInstall, opts: &LaunchOptions, patch_only: bool, steam_exe: Option<PathBuf>) -> Result<Plan, String> {
    let mut args = opts.args();
    match game.channel {
        Channel::Standalone => {
            if patch_only {
                args = vec!["-image".into()];
            }
            Ok(Plan::Program { exe: game.path.join(GAME_EXE), args, cwd: Some(game.path.clone()) })
        }
        Channel::Steam => {
            if patch_only {
                return Err("na Steam, o jogo é atualizado pela própria Steam".into());
            }
            match (args.is_empty(), steam_exe) {
                (true, _) => Ok(Plan::Url(format!("steam://rungameid/{STEAM_APP_ID}"))),
                (false, Some(steam)) => {
                    let mut a = vec!["-applaunch".to_string(), STEAM_APP_ID.to_string()];
                    a.extend(args);
                    Ok(Plan::Program { exe: steam, args: a, cwd: None })
                }
                (false, None) => Ok(Plan::Url(format!(
                    "steam://run/{STEAM_APP_ID}//{}/",
                    args.iter().map(|a| a.replace(' ', "%20")).collect::<Vec<_>>().join("%20")
                ))),
            }
        }
        Channel::Epic => {
            if patch_only {
                return Err("na Epic Games, o jogo é atualizado pelo próprio launcher da Epic".into());
            }
            let app = game.epic.as_ref().ok_or("não encontrei os dados da Epic Games desta instalação")?;
            let part = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !(part(&app.namespace) && part(&app.catalog_item_id) && part(&app.app_name)) || app.app_name.is_empty() {
                return Err("os dados da Epic Games desta instalação são inválidos".into());
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
        Plan::Program { exe, args, cwd } => {
            if !exe.is_file() {
                return Err(format!("{} não existe", exe.display()));
            }
            let mut cmd = Command::new(exe);
            cmd.args(args);
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            cmd.spawn().map(|_| ()).map_err(|e| format!("não consegui abrir {}: {e}", exe.display()))
        }
        Plan::Url(url) => shell_open(url),
    }
}

/// Hands a link or folder to Windows (`ShellExecuteW` "open").
pub fn shell_open(target: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (verb, file) = (w("open"), w(target));
    let h = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // ShellExecute returns a value above 32 on success.
    if h as usize > 32 {
        Ok(())
    } else {
        Err(format!("o Windows não conseguiu abrir {target} (código {})", h as usize))
    }
}

/// Every running `Gw2-64.exe`: its full path, or `None` when Windows does not tell (another user's process).
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

/// The `Gw2-64.exe` of `dir` is running (a copy whose path is unknown counts too: the safe side).
pub fn game_running_in(dir: &std::path::Path) -> bool {
    running_in(&running_games(), dir)
}

pub fn running_in(running: &[Option<PathBuf>], dir: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    running.iter().any(|p| match p {
        Some(exe) => exe.parent().is_some_and(|d| norm(d) == norm(dir)),
        None => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::EpicApp;

    fn install(channel: Channel) -> GameInstall {
        GameInstall { path: PathBuf::from(r"D:\GW2"), channel, source: String::new(), epic: None }
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
        let p = plan(&install(Channel::Standalone), &o, false, None).unwrap();
        assert_eq!(
            p,
            Plan::Program {
                exe: PathBuf::from(r"D:\GW2\Gw2-64.exe"),
                args: vec!["-shareArchive".into()],
                cwd: Some(PathBuf::from(r"D:\GW2"))
            }
        );
        let Plan::Program { args, .. } = plan(&install(Channel::Standalone), &o, true, None).unwrap() else { panic!() };
        assert_eq!(args, ["-image"]);
    }

    #[test]
    fn steam_goes_through_steam() {
        let none = LaunchOptions::default();
        let steam = Some(PathBuf::from(r"C:\Steam\steam.exe"));
        assert_eq!(
            plan(&install(Channel::Steam), &none, false, steam.clone()).unwrap(),
            Plan::Url("steam://rungameid/1284210".into())
        );
        let o = LaunchOptions { autologin: true, ..Default::default() };
        assert_eq!(
            plan(&install(Channel::Steam), &o, false, steam).unwrap(),
            Plan::Program {
                exe: PathBuf::from(r"C:\Steam\steam.exe"),
                args: vec!["-applaunch".into(), "1284210".into(), "-autologin".into()],
                cwd: None
            }
        );
        assert_eq!(
            plan(&install(Channel::Steam), &o, false, None).unwrap(),
            Plan::Url("steam://run/1284210//-autologin/".into())
        );
        assert!(plan(&install(Channel::Steam), &none, true, None).is_err());
    }

    #[test]
    fn only_the_game_in_that_folder_blocks_it() {
        let dir = std::path::Path::new(r"D:\GW2");
        assert!(!running_in(&[], dir));
        assert!(running_in(&[Some(PathBuf::from(r"d:\gw2\Gw2-64.exe"))], dir));
        assert!(!running_in(&[Some(PathBuf::from(r"E:\Steam\steamapps\common\Guild Wars 2\Gw2-64.exe"))], dir));
        assert!(running_in(&[None], dir), "a copy whose path is unknown may be this one");
        // This test process is not the game.
        assert!(!running_games().iter().flatten().any(|p| p == &std::env::current_exe().unwrap()));
    }

    #[test]
    fn epic_goes_through_the_epic_launcher() {
        let mut g = install(Channel::Epic);
        assert!(plan(&g, &LaunchOptions::default(), false, None).is_err());
        g.epic = Some(EpicApp { namespace: "ns1".into(), catalog_item_id: "abc".into(), app_name: "Gw2".into() });
        assert_eq!(
            plan(&g, &LaunchOptions::default(), false, None).unwrap(),
            Plan::Url("com.epicgames.launcher://apps/ns1%3Aabc%3AGw2?action=launch&silent=true".into())
        );
        g.epic = Some(EpicApp { namespace: String::new(), catalog_item_id: String::new(), app_name: "Gw2".into() });
        assert_eq!(
            plan(&g, &LaunchOptions::default(), false, None).unwrap(),
            Plan::Url("com.epicgames.launcher://apps/Gw2?action=launch&silent=true".into())
        );
        g.epic = Some(EpicApp { namespace: "a&b".into(), catalog_item_id: "x".into(), app_name: "y".into() });
        assert!(plan(&g, &LaunchOptions::default(), false, None).is_err());
    }
}
