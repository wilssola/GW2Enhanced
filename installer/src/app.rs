//! The window: a Tauri app whose page (ui/) calls these commands.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{Emitter, Manager, State};

use crate::games::{self, Channel, GameInstall};
use crate::github::{self, Release};
use crate::games::same_path as same;
use crate::launch;
use crate::proton::{self, ProtonStatus};
use crate::settings::Settings;
use crate::setup::{self, Status};

/// Download staging, inside the game folder (same volume, next to what the in-game updater uses).
const STAGE: [&str; 2] = ["GW2Enhanced", "installer-download"];

#[derive(Default)]
struct AppState {
    settings: Mutex<Settings>,
    installs: Mutex<Vec<GameInstall>>,
    releases: Mutex<Vec<Release>>,
    busy: AtomicBool,
}

/// One install as the page shows it.
#[derive(Serialize)]
struct InstallView {
    path: String,
    channel: Channel,
    source: String,
    status: Status,
    /// Its Gw2-64.exe is running.
    running: bool,
    /// Linux, Steam: the Proton override.
    proton: Option<ProtonStatus>,
    /// Linux, ArenaNet client: the Wine prefix.
    wine_prefix: Option<String>,
}

#[derive(Serialize)]
struct LocalPackage {
    path: String,
    version: Option<String>,
    package: Option<String>,
}

#[derive(Serialize)]
struct Overview {
    app_version: &'static str,
    /// "windows" or "linux".
    platform: &'static str,
    /// What the override line is (for the explanations on Linux).
    proton_line: &'static str,
    installs: Vec<InstallView>,
    selected: Option<String>,
    settings: Settings,
    /// The chosen install's game is running.
    game_running: bool,
    local_package: Option<LocalPackage>,
}

#[derive(Serialize)]
struct ReleasesView {
    releases: Vec<Release>,
    latest: Option<String>,
    /// The installed version, when known.
    installed: Option<String>,
    update_available: bool,
}

#[derive(Serialize, Clone)]
struct ProgressEvent {
    stage: String,
    done: u64,
    total: u64,
}

/// Holds the "busy" flag for one long operation.
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    fn take(flag: &'a AtomicBool) -> Result<Self, String> {
        if flag.swap(true, Ordering::SeqCst) {
            return Err("outra operação ainda está em andamento".into());
        }
        Ok(Self(flag))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn view(g: &GameInstall, running: &[Option<PathBuf>]) -> InstallView {
    InstallView {
        path: g.path.display().to_string(),
        channel: g.channel,
        source: g.source.clone(),
        status: setup::status(&g.path),
        running: launch::running_in(running, &g.path),
        proton: (cfg!(not(windows)) && g.channel == Channel::Steam).then(|| proton::status(g)),
        wine_prefix: g.wine_prefix.as_ref().map(|p| p.display().to_string()),
    }
}

/// Detected installs plus the folders picked by hand.
fn all_installs(settings: &Settings) -> Vec<GameInstall> {
    let mut list = games::find_installs();
    for dir in settings.manual_dirs.iter().chain(settings.game_dir.iter()) {
        if setup::is_game_dir(dir) && !list.iter().any(|g| same(&g.path, dir)) {
            list.push(games::classify(dir));
        }
    }
    list
}

/// The chosen install: the saved one if it is still there, else the first found.
fn selected(state: &AppState) -> Option<GameInstall> {
    let installs = lock(&state.installs);
    let settings = lock(&state.settings);
    settings
        .game_dir
        .as_ref()
        .and_then(|d| installs.iter().find(|g| same(&g.path, d)))
        .or_else(|| installs.first())
        .cloned()
}

fn find_install(state: &AppState, path: &str) -> Result<GameInstall, String> {
    lock(&state.installs)
        .iter()
        .find(|g| same(&g.path, Path::new(path)))
        .cloned()
        .ok_or_else(|| format!("{path} não está na lista de instalações"))
}

fn overview_of(state: &AppState) -> Overview {
    let settings = lock(&state.settings).clone();
    let running = launch::running_games();
    let installs: Vec<InstallView> = lock(&state.installs).iter().map(|g| view(g, &running)).collect();
    let selected = selected(state);
    Overview {
        app_version: env!("CARGO_PKG_VERSION"),
        platform: if cfg!(windows) { "windows" } else { "linux" },
        proton_line: proton::LAUNCH_LINE,
        installs,
        game_running: selected.as_ref().is_some_and(|g| launch::running_in(&running, &g.path)),
        selected: selected.map(|g| g.path.display().to_string()),
        settings,
        local_package: setup::local_package().map(|p| {
            let info = setup::dll_info(&p.join(setup::DLL));
            LocalPackage {
                path: p.display().to_string(),
                version: info.as_ref().map(|i| i.0.clone()),
                package: info.map(|i| i.1).filter(|p| !p.is_empty()),
            }
        }),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn overview(app: tauri::AppHandle, rescan: bool) -> Result<Overview, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        if rescan || lock(&state.installs).is_empty() {
            let settings = lock(&state.settings).clone();
            *lock(&state.installs) = all_installs(&settings);
        }
        Ok(overview_of(&state))
    })
    .await
}

#[tauri::command]
fn select_game(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let g = find_install(&state, &path)?;
    let mut s = lock(&state.settings);
    s.game_dir = Some(g.path);
    s.save()
}

#[tauri::command]
async fn browse_game(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<Option<String>, String> {
    #[cfg(windows)]
    let owner = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    #[cfg(not(windows))]
    let owner = {
        let _ = &window;
        0
    };
    blocking(move || {
        let Some(dir) = crate::dialog::pick_game_exe(owner)? else { return Ok(None) };
        if !setup::is_game_dir(&dir) {
            return Err(format!("{} não tem o Gw2-64.exe", dir.display()));
        }
        let state = app.state::<AppState>();
        let g = games::classify(&dir);
        {
            let mut installs = lock(&state.installs);
            if !installs.iter().any(|i| same(&i.path, &dir)) {
                installs.push(g);
            }
        }
        let mut s = lock(&state.settings);
        if !s.manual_dirs.iter().any(|d| same(d, &dir)) {
            s.manual_dirs.push(dir.clone());
        }
        s.game_dir = Some(dir.clone());
        s.save()?;
        Ok(Some(dir.display().to_string()))
    })
    .await
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    if !gw2enhanced_core::release::PACKAGES.contains(&settings.package.as_str()) {
        return Err(format!("pacote desconhecido: {}", settings.package));
    }
    let mut s = lock(&state.settings);
    // The game folder lists are managed by select_game / browse_game.
    let keep = (s.game_dir.clone(), s.manual_dirs.clone());
    *s = Settings { game_dir: keep.0, manual_dirs: keep.1, ..settings };
    s.save()
}

#[tauri::command]
async fn releases(app: tauri::AppHandle) -> Result<ReleasesView, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let list = github::list_releases()?;
        let installed = selected(&state).and_then(|g| setup::status(&g.path).version);
        let latest = github::latest_stable(&list).map(|r| r.version.clone());
        let update_available = match (&installed, &latest) {
            (Some(i), Some(l)) => github::is_newer(i, l),
            _ => false,
        };
        *lock(&state.releases) = list.clone();
        Ok(ReleasesView { releases: list, latest, installed, update_available })
    })
    .await
}

/// The game in `dir` must be closed to replace its files (another copy elsewhere is fine).
fn ensure_closed(dir: &Path) -> Result<(), String> {
    if launch::game_running_in(dir) {
        return Err("feche o Guild Wars 2 antes de continuar".into());
    }
    Ok(())
}

#[tauri::command]
async fn install_release(app: tauri::AppHandle, tag: String, package: String) -> Result<Vec<String>, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let _busy = Busy::take(&state.busy)?;
        let game = selected(&state).ok_or("escolha a pasta do jogo primeiro")?;
        ensure_closed(&game.path)?;
        let release = lock(&state.releases)
            .iter()
            .find(|r| r.tag == tag)
            .cloned()
            .ok_or("atualize a lista de versões e tente de novo")?;
        let stage = STAGE.iter().fold(game.path.clone(), |p, part| p.join(part));
        let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let mut progress = |stage: &str, done: u64, total: u64| {
            // At most ~20 events a second.
            if stage != "download" || last.elapsed().as_millis() >= 50 || (total != 0 && done == total) {
                last = std::time::Instant::now();
                let _ = app.emit("progress", ProgressEvent { stage: stage.into(), done, total });
            }
        };
        let result = github::fetch_package(&release, &package, &stage, &mut progress).and_then(|files| {
            progress("install", 0, 0);
            ensure_closed(&game.path)?;
            setup::install(&files, &game.path)
        });
        let _ = std::fs::remove_dir_all(&stage);
        let mut log = result?;
        log.insert(0, format!("GW2 Enhanced {} ({package}) baixado e verificado", release.version));
        let mut s = lock(&state.settings);
        s.package = package;
        let _ = s.save();
        Ok(log)
    })
    .await
}

#[tauri::command]
async fn install_local(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let _busy = Busy::take(&state.busy)?;
        let game = selected(&state).ok_or("escolha a pasta do jogo primeiro")?;
        let pkg = setup::local_package().ok_or("não há um pacote ao lado do instalador")?;
        ensure_closed(&game.path)?;
        setup::install(&pkg, &game.path)
    })
    .await
}

#[tauri::command]
async fn uninstall(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let _busy = Busy::take(&state.busy)?;
        let game = selected(&state).ok_or("escolha a pasta do jogo primeiro")?;
        ensure_closed(&game.path)?;
        setup::uninstall(&game.path)
    })
    .await
}

#[tauri::command]
async fn play(app: tauri::AppHandle, path: String, patch_only: bool) -> Result<String, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let game = find_install(&state, &path)?;
        let opts = lock(&state.settings).launch.clone();
        let plan = launch::plan(&game, &opts, patch_only, &launch::Tools::find(&game))?;
        launch::run(&plan)?;
        let how = match game.channel {
            Channel::Steam if cfg!(not(windows)) => "pela Steam (Proton)",
            Channel::Steam => "pela Steam",
            Channel::Standalone if cfg!(not(windows)) && !patch_only => "pelo Wine",
            Channel::Epic => "pela Epic Games",
            Channel::Standalone if patch_only => "para atualizar",
            Channel::Standalone => "pelo Gw2-64.exe",
        };
        if lock(&state.settings).close_on_launch && !patch_only {
            let app2 = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                app2.exit(0);
            });
        }
        Ok(format!("Guild Wars 2 iniciado {how}"))
    })
    .await
}

/// Adds the Proton override to the game's Steam launch options (Linux, Steam closed).
#[tauri::command]
async fn set_proton_override(app: tauri::AppHandle, path: String) -> Result<Vec<String>, String> {
    blocking(move || {
        let state = app.state::<AppState>();
        let game = find_install(&state, &path)?;
        if game.channel != Channel::Steam {
            return Err("isto só vale para o jogo da Steam".into());
        }
        proton::apply(&game)
    })
    .await
}

/// Opens a release page or a link from the notes in the browser (http(s) only).
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) || url.chars().any(char::is_control) {
        return Err("link recusado".into());
    }
    launch::shell_open(&url)
}

/// Opens a game folder in Explorer (only one of the listed installs).
#[tauri::command]
fn open_folder(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let g = find_install(&state, &path)?;
    launch::shell_open(&g.path.display().to_string())
}

pub fn run() {
    let settings = Settings::load();
    tauri::Builder::default()
        .manage(AppState { settings: Mutex::new(settings), ..Default::default() })
        .invoke_handler(tauri::generate_handler![
            overview,
            select_game,
            browse_game,
            save_settings,
            releases,
            install_release,
            install_local,
            uninstall,
            play,
            set_proton_override,
            open_url,
            open_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running the installer window");
}

/// Kept for the command line: the folder the settings point at, when it is a game folder.
pub fn saved_game_dir() -> Option<PathBuf> {
    Settings::load().game_dir.filter(|d| setup::is_game_dir(d))
}
