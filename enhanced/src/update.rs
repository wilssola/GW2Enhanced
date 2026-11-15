//! Auto-update against the GitHub releases (README, "Updates").
//!
//! A few seconds after the game starts, a background thread asks GitHub for the latest release. When it is newer,
//! the overlay shows a small window of its own (even with the main panel closed): "Update now" downloads the same
//! package this DLL was built as (`GW2E_PACKAGE`, set by CI), checks it against the release's `SHA256SUMS.txt`,
//! unpacks it next to the game (same volume) and swaps every file in with renames; then the game is closed so the
//! next start runs the new version.
//!
//! Files that are in use (this `msimg32.dll`, the running translator and its DLLs) cannot be overwritten on Windows,
//! but they can be renamed: every file that is replaced is first renamed to `<name>.old`, and the ones that cannot be
//! deleted right away are listed in `GW2Enhanced\update_cleanup.txt` and deleted at the next start. If any step
//! fails, the renames already done are undone and the game keeps running on the old version.
//!
//! Nothing here blocks the game: the check, the download and the install run on their own threads; the overlay only
//! reads the shared state.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gw2enhanced_core::i18n::tr;
use gw2enhanced_core::registry::{self, State};
use gw2enhanced_core::release::{
    OWNER, PACKAGES, REPO, SIG_ASSET, SUMS_ASSET, asset_name, asset_url_ok, expected_sum, is_protected, safe_relative,
};
use gw2enhanced_core::trf;
use hudhook::imgui::{Condition, Ui, WindowFlags};

/// How long after the start the check runs: the game is busy loading before that.
const CHECK_DELAY: Duration = Duration::from_secs(5);
/// How long the "updated" message stays before the game is closed.
const CLOSE_AFTER: Duration = Duration::from_secs(10);
/// After asking the game window to close: if the process is still alive by then, it is ended.
const CLOSE_GRACE: Duration = Duration::from_secs(20);

/// Our folder next to `Gw2-64.exe` (config, log, staging).
const DATA_DIR: &str = "GW2Enhanced";
const STAGE_DIR: &str = "update";
const CLEANUP_LIST: &str = "update_cleanup.txt";
const LOG_FILE: &str = "update_log.txt";

/// Size caps: the largest package (CUDA translator runtime) is well under this.
const MAX_DOWNLOAD: u64 = 6 * 1024 * 1024 * 1024;
const MAX_UNPACKED: u64 = 12 * 1024 * 1024 * 1024;

/// The package this DLL was built as, or `None` for a local build (no updates then).
pub fn package() -> Option<&'static str> {
    option_env!("GW2E_PACKAGE").map(str::trim).filter(|p| !p.is_empty())
}

/// The version this DLL was built as: CI's `GW2E_VERSION` (the release tag), else the crate version.
pub fn current_version() -> &'static str {
    option_env!("GW2E_VERSION")
        .map(|v| v.trim().trim_start_matches('v'))
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// `Some(version)` (without `v`) when `tag` is newer than `current`. Build metadata (`+...`) never makes a release
/// newer; a pre-release is older than its release.
fn release_update(current: &str, tag: &str) -> Result<Option<String>, String> {
    let version = tag.trim().trim_start_matches('v');
    let current_order = current.split('+').next().unwrap_or(current);
    let version_order = version.split('+').next().unwrap_or(version);
    let newer = self_update::version::bump_is_greater(current_order, version_order)
        .map_err(|_| format!("invalid version (release {tag:?}, this build {current:?})"))?;
    Ok(newer.then(|| version.to_string()))
}

/// `check_updates` in `GW2Enhanced\config.ini` (`key=value` lines, `#`/`;` comments); on unless it says no.
fn updates_enabled(config: &str) -> bool {
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("check_updates")
        {
            let v = v.trim().to_ascii_lowercase();
            return !matches!(v.as_str(), "false" | "0" | "no" | "off");
        }
    }
    true
}

/// `<path>.old`, or `<path>.<n>.old` when an earlier `.old` is still there and cannot be deleted (in use).
fn free_old_path(dest: &Path) -> Result<PathBuf, String> {
    let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
    for n in 0..10 {
        let old = if n == 0 {
            dest.with_file_name(format!("{name}.old"))
        } else {
            dest.with_file_name(format!("{name}.{n}.old"))
        };
        if !old.exists() || fs::remove_file(&old).is_ok() {
            return Ok(old);
        }
    }
    Err(format!("{} has too many old copies in use", dest.display()))
}

fn same_content(a: &Path, b: &Path) -> bool {
    use sha2::Digest as _;
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    let sum = |p: &Path| -> std::io::Result<Vec<u8>> {
        let mut h = sha2::Sha256::new();
        std::io::copy(&mut fs::File::open(p)?, &mut h)?;
        Ok(h.finalize().to_vec())
    };
    matches!((sum(a), sum(b)), (Ok(x), Ok(y)) if x == y)
}

/// One file put in place: where, and where the previous one was moved to (`None`: there was none).
struct Placed {
    dest: PathBuf,
    old: Option<PathBuf>,
}

/// What a successful install leaves: the previous versions that could not be deleted yet (in use).
#[derive(Debug, Default, PartialEq)]
struct Installed {
    replaced: usize,
    unchanged: usize,
    pending_old: Vec<PathBuf>,
}

/// Moves every `files` entry (relative) from `stage` into `game`. A file already there is renamed to `.old` first
/// (this works for a loaded DLL or a running exe). On any failure everything done so far is undone.
fn install(stage: &Path, files: &[PathBuf], game: &Path) -> Result<Installed, String> {
    let mut journal: Vec<Placed> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    let mut unchanged = 0;
    let mut step = || -> Result<(), String> {
        for rel in files {
            let src = stage.join(rel);
            let dest = game.join(rel);
            if let Some(parent) = dest.parent() {
                // Remember the folders this creates so a rollback removes them again.
                let mut missing = Vec::new();
                let mut p = parent;
                while !p.exists() && p.starts_with(game) && p != game {
                    missing.push(p.to_path_buf());
                    match p.parent() {
                        Some(up) => p = up,
                        None => break,
                    }
                }
                fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                created_dirs.extend(missing.into_iter().rev());
            }
            if dest.is_dir() {
                return Err(format!("{} is a folder", dest.display()));
            }
            if dest.is_file() {
                if same_content(&src, &dest) {
                    unchanged += 1;
                    continue;
                }
                let old = free_old_path(&dest)?;
                fs::rename(&dest, &old).map_err(|e| format!("cannot move {} aside: {e}", dest.display()))?;
                journal.push(Placed { dest: dest.clone(), old: Some(old) });
                fs::rename(&src, &dest).map_err(|e| format!("cannot put {} in place: {e}", dest.display()))?;
            } else {
                fs::rename(&src, &dest).map_err(|e| format!("cannot put {} in place: {e}", dest.display()))?;
                journal.push(Placed { dest: dest.clone(), old: None });
            }
        }
        Ok(())
    };
    if let Err(e) = step() {
        let mut problems = Vec::new();
        for p in journal.iter().rev() {
            // The new file is never in use yet, so it can always be removed; the old one goes back by rename.
            if p.dest.is_file()
                && let Err(err) = fs::remove_file(&p.dest)
            {
                problems.push(format!("{}: {err}", p.dest.display()));
                continue;
            }
            if let Some(old) = &p.old
                && let Err(err) = fs::rename(old, &p.dest)
            {
                problems.push(format!("{} (left at {}): {err}", p.dest.display(), old.display()));
            }
        }
        for d in created_dirs.iter().rev() {
            let _ = fs::remove_dir(d);
        }
        return Err(if problems.is_empty() {
            format!("{e} (nothing was changed)")
        } else {
            format!("{e}; restoring failed for {}", problems.join(", "))
        });
    }
    let mut pending_old = Vec::new();
    for old in journal.iter().filter_map(|p| p.old.as_ref()) {
        // In use (the running DLL, the translator): deleted at the next start instead.
        if fs::remove_file(old).is_err() {
            pending_old.push(old.clone());
        }
    }
    Ok(Installed { replaced: journal.len(), unchanged, pending_old })
}

/// Deletes the `.old` files a previous update could not delete (listed in `list`, relative to `game`). Returns what
/// is still left.
fn sweep_listed(game: &Path, list: &str) -> Vec<String> {
    let mut left = Vec::new();
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(rel) = safe_relative(line) else { continue };
        if !line.ends_with(".old") {
            continue;
        }
        let p = game.join(rel);
        if p.is_file() && fs::remove_file(&p).is_err() {
            left.push(line.to_string());
        }
    }
    left
}

// ---------------------------------------------------------------------------------------------------------------
// State shared with the overlay.

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Not checked yet.
    Idle,
    /// No check this start (development build, turned off, not the game): the text says why.
    Disabled(String),
    UpToDate,
    CheckFailed(String),
    Available { version: String },
    /// The player said "Later".
    Dismissed { version: String },
    Downloading { version: String },
    Installing { version: String },
    Done { version: String, at: Instant },
    Failed { version: String, error: String },
}

static PHASE: Mutex<Phase> = Mutex::new(Phase::Idle);
static DOWNLOADED: AtomicU64 = AtomicU64::new(0);
static DOWNLOAD_SIZE: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
static CLOSE_NOW: AtomicBool = AtomicBool::new(false);
static RELEASE: Mutex<Option<Release>> = Mutex::new(None);

fn phase() -> Phase {
    PHASE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_phase(p: Phase) {
    let health = match &p {
        Phase::Idle => None,
        Phase::Disabled(why) => Some(State::Off(why.clone())),
        Phase::UpToDate => Some(State::Ok),
        Phase::CheckFailed(e) => Some(State::Degraded(format!("could not check for updates: {e}"))),
        Phase::Available { version } | Phase::Dismissed { version } => Some(State::Degraded(format!(
            "{version} is available (you have {})",
            current_version()
        ))),
        Phase::Downloading { version } => Some(State::Degraded(format!("downloading {version}"))),
        Phase::Installing { version } => Some(State::Degraded(format!("installing {version}"))),
        Phase::Done { version, .. } => Some(State::Degraded(format!("updated to {version}: restart the game"))),
        Phase::Failed { version, error } => Some(State::Degraded(format!("update to {version} failed: {error}"))),
    };
    if let Some(s) = health {
        registry::report("update", "", s);
    }
    *PHASE.lock().unwrap_or_else(|e| e.into_inner()) = p;
}

/// The update window is on screen (the overlay then takes the input and draws the cursor).
pub fn window_visible() -> bool {
    matches!(
        *PHASE.lock().unwrap_or_else(|e| e.into_inner()),
        Phase::Available { .. }
            | Phase::Downloading { .. }
            | Phase::Installing { .. }
            | Phase::Done { .. }
            | Phase::Failed { .. }
    )
}

/// One line for the Health tab's footer (in the overlay language; the reasons and errors stay as reported).
pub fn summary() -> String {
    let pkg = package().unwrap_or(tr("development build", "build de desenvolvimento"));
    let state = match phase() {
        Phase::Idle => tr("update check pending", "verificação de atualização pendente").to_string(),
        Phase::Disabled(why) => why,
        Phase::UpToDate => tr("up to date", "atualizado").to_string(),
        Phase::CheckFailed(e) => trf!("update check failed: {e}", "falha ao verificar atualização: {e}"),
        Phase::Available { version } | Phase::Dismissed { version } => {
            trf!("{version} available", "{version} disponível")
        }
        Phase::Downloading { version } | Phase::Installing { version } => {
            trf!("updating to {version}", "atualizando para {version}")
        }
        Phase::Done { version, .. } => {
            trf!("updated to {version}, restart the game", "atualizado para {version}, reinicie o jogo")
        }
        Phase::Failed { error, .. } => trf!("update failed: {error}", "falha na atualização: {error}"),
    };
    format!("GW2 Enhanced v{} ({pkg}): {state}", current_version())
}

// ---------------------------------------------------------------------------------------------------------------
// Files and folders.

fn game_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.to_path_buf())
}

fn is_game_exe() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("Gw2-64.exe")))
        .unwrap_or(false)
}

fn log(msg: &str) {
    let Some(dir) = game_dir().map(|g| g.join(DATA_DIR)) else { return };
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(LOG_FILE);
    // Kept small: one start writes a line or two.
    if fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

/// Deletes what the previous update left: the staging folder and the `.old` files that were in use.
fn sweep(game: &Path) {
    let data = game.join(DATA_DIR);
    let _ = fs::remove_dir_all(data.join(STAGE_DIR));
    let list_path = data.join(CLEANUP_LIST);
    let mut left = Vec::new();
    if let Ok(list) = fs::read_to_string(&list_path) {
        left = sweep_listed(game, &list);
    }
    // The common case even without a list.
    let _ = fs::remove_file(game.join("msimg32.dll.old"));
    if left.is_empty() {
        let _ = fs::remove_file(&list_path);
    } else {
        let _ = fs::write(&list_path, left.join("\n") + "\n");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// GitHub.

/// While the repository is private, the API needs a token (`GW2E_GH_TOKEN` in the environment the game starts in);
/// once public this works unauthenticated. A token is never built into the DLL.
fn token() -> Option<String> {
    std::env::var("GW2E_GH_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

#[derive(Clone)]
struct Asset {
    name: String,
    /// The API url: works for private repositories too (with the token), unlike `browser_download_url`.
    url: String,
    size: u64,
}

#[derive(Clone)]
struct Release {
    version: String,
    assets: Vec<Asset>,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

fn parse_release(text: &str) -> Result<Release, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("invalid release data: {e}"))?;
    let version = json
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or("the latest release has no tag")?
        .trim()
        .trim_start_matches('v')
        .to_string();
    let assets = json
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("url")?.as_str()?.to_string(),
                        size: a.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { version, assets })
}

fn get(url: &str, accept: &str, api: bool) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut req = ureq::get(url)
        .header("User-Agent", "GW2Enhanced-updater")
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(t) = token() {
        // ureq drops it on redirects to another host (the asset storage), as it should.
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    req.config()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .timeout_global(api.then(|| Duration::from_secs(30)))
        .build()
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) if token().is_none() => {
                "HTTP 404 (the repository is private: set GW2E_GH_TOKEN, or no release exists)".to_string()
            }
            ureq::Error::StatusCode(c @ (401 | 403)) => format!("HTTP {c} (token refused or rate limited)"),
            e => e.to_string(),
        })
}

fn latest_release() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest");
    let text = get(&url, "application/vnd.github+json", true)?
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    parse_release(&text)
}

/// Streams `asset` into `path`, counting into DOWNLOADED; returns its SHA-256 (lowercase hex).
fn download_to(asset: &Asset, path: &Path) -> Result<String, String> {
    use sha2::Digest as _;
    let mut resp = get(&asset.url, "application/octet-stream", false)?;
    let mut reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let file = fs::File::create(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| format!("cannot save the download (disk full?): {e}"))?;
        total += n as u64;
        DOWNLOADED.store(total, Ordering::Relaxed);
    }
    out.flush().map_err(|e| format!("cannot save the download (disk full?): {e}"))?;
    if asset.size != 0 && total != asset.size {
        return Err(format!("the download is incomplete ({total} of {} bytes)", asset.size));
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpacks `zip` into `into`, refusing unsafe names; returns the files (relative), protected ones left out.
fn unpack(zip: &Path, into: &Path) -> Result<Vec<PathBuf>, String> {
    let file = fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| format!("bad zip: {e}"))?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut unpacked = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("bad zip: {e}"))?;
        let name = entry.name().map_err(|e| format!("bad zip: {e}"))?.into_owned();
        let rel = safe_relative(&name)
            .filter(|_| entry.enclosed_name().is_some())
            .ok_or_else(|| format!("the zip has an unsafe path: {name:?}"))?;
        if entry.is_dir() {
            continue;
        }
        if !entry.is_file() || entry.is_symlink() {
            return Err(format!("the zip has a link or special file: {name:?}"));
        }
        if is_protected(&rel) {
            log(&format!("skipped {name} (never overwritten by an update)"));
            continue;
        }
        if !seen.insert(rel.to_string_lossy().to_ascii_lowercase()) {
            return Err(format!("the zip has {name:?} twice"));
        }
        let size = entry.size();
        unpacked += size;
        if unpacked > MAX_UNPACKED {
            return Err("the zip unpacks to more than expected".into());
        }
        let dest = into.join(&rel);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).map_err(|e| format!("cannot unpack (disk full?): {e}"))?;
        }
        let mut out = fs::File::create(&dest).map_err(|e| format!("cannot unpack (disk full?): {e}"))?;
        let written = std::io::copy(&mut (&mut entry).take(size + 1), &mut out)
            .map_err(|e| format!("cannot unpack {name}: {e}"))?;
        if written != size {
            return Err(format!("cannot unpack {name}: size mismatch"));
        }
        files.push(rel);
    }
    if files.is_empty() {
        return Err("the zip is empty".into());
    }
    // The proxy DLL last: if the game were killed half way, the rest is already new.
    files.sort_by_key(|r| r.to_string_lossy().eq_ignore_ascii_case("msimg32.dll"));
    Ok(files)
}

// ---------------------------------------------------------------------------------------------------------------
// The two background jobs.

/// Starts the once-per-start check on its own thread (call from the init thread, never under the loader lock).
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("gw2enhanced-update".into()).spawn(|| {
        std::thread::sleep(CHECK_DELAY);
        if std::panic::catch_unwind(check).is_err() {
            set_phase(Phase::CheckFailed("the check panicked".into()));
        }
    });
}

fn check() {
    if !is_game_exe() {
        set_phase(Phase::Disabled("not running inside Gw2-64.exe".into()));
        return;
    }
    let Some(game) = game_dir() else {
        set_phase(Phase::Disabled("cannot find the game folder".into()));
        return;
    };
    sweep(&game);
    let Some(pkg) = package() else {
        log("development build (GW2E_PACKAGE not set at build time): no update check");
        set_phase(Phase::Disabled("development build: no update check".into()));
        return;
    };
    if !PACKAGES.contains(&pkg) {
        log(&format!("unknown package {pkg:?}: no update check"));
        set_phase(Phase::Disabled(format!("unknown package {pkg:?}: no update check")));
        return;
    }
    let config = fs::read_to_string(game.join(DATA_DIR).join("config.ini")).unwrap_or_default();
    if !updates_enabled(&config) {
        log("check_updates=false in GW2Enhanced\\config.ini: no update check");
        set_phase(Phase::Disabled("turned off (check_updates=false in GW2Enhanced\\config.ini)".into()));
        return;
    }
    let release = match latest_release() {
        Ok(r) => r,
        Err(e) => {
            log(&format!("check failed: {e}"));
            set_phase(Phase::CheckFailed(e));
            return;
        }
    };
    let newer = match release_update(current_version(), &release.version) {
        Ok(n) => n,
        Err(e) => {
            log(&format!("check failed: {e}"));
            set_phase(Phase::CheckFailed(e));
            return;
        }
    };
    let Some(version) = newer else {
        log(&format!("up to date ({} {pkg}; latest {})", current_version(), release.version));
        set_phase(Phase::UpToDate);
        return;
    };
    let zip = asset_name(pkg, &version);
    if release.asset(&zip).is_none() {
        let e = format!("release {version} has no {zip}");
        log(&e);
        set_phase(Phase::CheckFailed(e));
        return;
    }
    log(&format!("{version} is available (this is {} {pkg})", current_version()));
    *RELEASE.lock().unwrap_or_else(|e| e.into_inner()) = Some(release);
    set_phase(Phase::Available { version });
}

/// "Update now": downloads, verifies and installs on its own thread, then closes the game.
fn begin_update(version: String) {
    DOWNLOADED.store(0, Ordering::Relaxed);
    set_phase(Phase::Downloading { version: version.clone() });
    let _ = std::thread::Builder::new().name("gw2enhanced-update-apply".into()).spawn(move || {
        let r = std::panic::catch_unwind(|| apply(&version)).unwrap_or_else(|_| Err("the update panicked".into()));
        match r {
            Ok(()) => {
                log(&format!("updated to {version}; closing the game"));
                let at = Instant::now();
                set_phase(Phase::Done { version, at });
                while at.elapsed() < CLOSE_AFTER && !CLOSE_NOW.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                close_game();
            }
            Err(error) => {
                log(&format!("update to {version} failed: {error}"));
                set_phase(Phase::Failed { version, error });
            }
        }
    });
}

fn apply(version: &str) -> Result<(), String> {
    let _lock = InstanceLock::acquire()?;
    let release = RELEASE.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("no release")?;
    let pkg = package().ok_or("development build")?;
    let game = game_dir().ok_or("cannot find the game folder")?;
    let zip_name = asset_name(pkg, version);
    let asset = release.asset(&zip_name).ok_or(format!("no {zip_name}"))?.clone();
    let sums = release
        .asset(SUMS_ASSET)
        .ok_or(format!("release {version} has no {SUMS_ASSET}: refusing an unverified update"))?
        .clone();
    let sig = release
        .asset(SIG_ASSET)
        .ok_or(format!("release {version} has no {SIG_ASSET}: refusing an unsigned update"))?
        .clone();
    for a in [&asset, &sums, &sig] {
        if !asset_url_ok(&a.url) {
            return Err(format!("{} is not served by this repository's releases", a.name));
        }
    }
    DOWNLOAD_SIZE.store(asset.size, Ordering::Relaxed);

    // Staged inside the game folder: the same volume, so the swap is renames only.
    let stage = game.join(DATA_DIR).join(STAGE_DIR);
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).map_err(|e| format!("cannot write to the game folder: {e}"))?;
    let result = (|| {
        let sums_bytes = {
            let mut resp = get(&sums.url, "application/octet-stream", false)?;
            resp.body_mut().with_config().limit(1024 * 1024).read_to_vec().map_err(|e| e.to_string())?
        };
        let sig_text = {
            let mut resp = get(&sig.url, "application/octet-stream", false)?;
            resp.body_mut().with_config().limit(4096).read_to_string().map_err(|e| e.to_string())?
        };
        // The signature covers the exact bytes: checked before anything in them is trusted.
        gw2enhanced_core::signed::verify(&sums_bytes, &sig_text)?;
        let sums_text = String::from_utf8(sums_bytes).map_err(|_| format!("{SUMS_ASSET} is not text"))?;
        let expected = expected_sum(&sums_text, &zip_name).ok_or(format!("{zip_name} is not in {SUMS_ASSET}"))?;
        let zip_path = stage.join(&zip_name);
        let actual = download_to(&asset, &zip_path)?;
        if actual != expected {
            return Err("the download does not match its checksum".to_string());
        }
        if let Phase::Downloading { version } = phase() {
            set_phase(Phase::Installing { version });
        }
        let files_dir = stage.join("files");
        let files = unpack(&zip_path, &files_dir)?;
        let done = install(&files_dir, &files, &game)?;
        log(&format!(
            "installed {version}: {} files replaced, {} unchanged, {} old copies in use",
            done.replaced,
            done.unchanged,
            done.pending_old.len()
        ));
        if !done.pending_old.is_empty() {
            let list_path = game.join(DATA_DIR).join(CLEANUP_LIST);
            let mut list = fs::read_to_string(&list_path).unwrap_or_default();
            for p in &done.pending_old {
                if let Ok(rel) = p.strip_prefix(&game) {
                    list.push_str(&rel.to_string_lossy());
                    list.push('\n');
                }
            }
            let _ = fs::write(&list_path, list);
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

/// One update at a time when several game windows run from the same folder.
struct InstanceLock(windows_sys::Win32::Foundation::HANDLE);

impl InstanceLock {
    fn acquire() -> Result<Self, String> {
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "Local\\GW2Enhanced-Update\0".encode_utf16().collect();
        let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        if h.is_null() {
            return Err("cannot create the update lock".into());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
            return Err("another game window is updating".into());
        }
        Ok(Self(h))
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Asks the game to close the way Alt+F4 / the window's X does (it saves its settings and leaves the server
/// cleanly); if it is still running after CLOSE_GRACE, the process is ended. `TerminateProcess` rather than
/// `std::process::exit`: exit runs every DLL's detach under the loader lock while the game's threads are still
/// running, which can hang; the files are already in place, so nothing is lost by not unwinding.
fn close_game() {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, TRUE};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
    };
    unsafe extern "system" fn each(hwnd: HWND, _: LPARAM) -> i32 {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == GetCurrentProcessId() && IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER).is_null() {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        TRUE
    }
    unsafe { EnumWindows(Some(each), 0) };
    std::thread::sleep(CLOSE_GRACE);
    log("the game did not close by itself: ending it");
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        TerminateProcess(GetCurrentProcess(), 0);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The window.

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Draws the update window when there is something to say (call every frame, main panel open or not).
pub fn draw(ui: &Ui) {
    let p = phase();
    if !matches!(
        p,
        Phase::Available { .. }
            | Phase::Downloading { .. }
            | Phase::Installing { .. }
            | Phase::Done { .. }
            | Phase::Failed { .. }
    ) {
        return;
    }
    let [w, _] = ui.io().display_size;
    ui.window(tr("GW2 Enhanced update###gw2enhanced-update", "Atualização do GW2 Enhanced###gw2enhanced-update"))
        .position([w * 0.5, 80.0], Condition::Appearing)
        .position_pivot([0.5, 0.0])
        .size([420.0, 0.0], Condition::Appearing)
        .flags(WindowFlags::NO_COLLAPSE | WindowFlags::ALWAYS_AUTO_RESIZE | WindowFlags::NO_SAVED_SETTINGS)
        .build(|| match &p {
            Phase::Available { version } => {
                let current = current_version();
                ui.text_wrapped(trf!(
                    "GW2 Enhanced {version} is available (you have {current}).",
                    "O GW2 Enhanced {version} está disponível (você tem a {current})."
                ));
                ui.text_disabled(tr(
                    "The game closes after the update; start it again to use the new version.",
                    "O jogo fecha depois da atualização; abra de novo para usar a nova versão.",
                ));
                if ui.button(tr("Update now", "Atualizar agora")) {
                    begin_update(version.clone());
                }
                ui.same_line();
                if ui.button(tr("Later", "Depois")) {
                    log(&format!("{version}: later"));
                    set_phase(Phase::Dismissed { version: version.clone() });
                }
            }
            Phase::Downloading { version } => {
                let done = DOWNLOADED.load(Ordering::Relaxed);
                let size = DOWNLOAD_SIZE.load(Ordering::Relaxed);
                if size > 0 {
                    let pct = (done as f64 * 100.0 / size as f64).min(100.0);
                    let (done, size) = (mb(done), mb(size));
                    ui.text(trf!(
                        "Downloading {version}: {done} of {size} ({pct:.0}%)",
                        "Baixando {version}: {done} de {size} ({pct:.0}%)"
                    ));
                } else {
                    let done = mb(done);
                    ui.text(trf!("Downloading {version}: {done}", "Baixando {version}: {done}"));
                }
                ui.text_disabled(tr("You can keep playing meanwhile.", "Você pode continuar jogando enquanto isso."));
            }
            Phase::Installing { version } => {
                ui.text(trf!("Installing {version}...", "Instalando {version}..."));
            }
            Phase::Done { version, at } => {
                let left = CLOSE_AFTER.saturating_sub(at.elapsed()).as_secs() + 1;
                let left = if CLOSE_NOW.load(Ordering::Relaxed) || at.elapsed() >= CLOSE_AFTER { 0 } else { left };
                ui.text_wrapped(trf!(
                    "Updated to {version}. The game will close in {left} s; start it again.",
                    "Atualizado para {version}. O jogo vai fechar em {left} s; abra de novo depois."
                ));
                if ui.button(tr("Close now", "Fechar agora")) {
                    CLOSE_NOW.store(true, Ordering::Relaxed);
                }
            }
            Phase::Failed { version, error } => {
                ui.text_wrapped(trf!(
                    "The update to {version} failed: {error}",
                    "A atualização para {version} falhou: {error}"
                ));
                ui.text_disabled(tr(
                    "Nothing was replaced; the game keeps running on the current version.",
                    "Nada foi substituído; o jogo continua na versão atual.",
                ));
                if ui.button("OK") {
                    set_phase(Phase::Dismissed { version: version.clone() });
                }
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gw2e_update_test_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(p: &Path, data: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    #[test]
    fn versions_use_semver_ordering() {
        for tag in ["1.2.4", "v1.3.0", "v1.10.0", "2.0.0"] {
            assert_eq!(release_update("1.2.3", tag).unwrap(), Some(tag.trim_start_matches('v').to_string()));
        }
        for tag in ["1.2.3", "v1.2.3", "1.2.2", "v1.1.10", "1.2.3-beta.1", "1.2.3+build.9"] {
            assert_eq!(release_update("1.2.3", tag).unwrap(), None, "{tag}");
        }
        assert_eq!(release_update("1.2.3-beta.1", "v1.2.3").unwrap(), Some("1.2.3".into()));
        for tag in ["", "v", "latest", "1.2", "v1.2.3.4"] {
            assert!(release_update("1.2.3", tag).is_err(), "{tag}");
        }
        // This build's own version is valid and never newer than itself.
        assert_eq!(release_update(current_version(), &format!("v{}", current_version())).unwrap(), None);
    }

    #[test]
    fn this_build_is_a_known_package() {
        if let Some(p) = package() {
            assert!(PACKAGES.contains(&p), "GW2E_PACKAGE={p} is not a known package");
        }
    }

    #[test]
    fn config_opt_out() {
        assert!(updates_enabled(""));
        assert!(updates_enabled("other=1\ncheck_updates = true\n"));
        assert!(!updates_enabled("check_updates=false"));
        assert!(!updates_enabled("# comment\n Check_Updates = 0 \n"));
        assert!(updates_enabled("# check_updates=false\n;check_updates=false"));
    }

    #[test]
    fn install_replaces_adds_and_keeps_identical_files() {
        let dir = scratch("install");
        let (stage, game) = (dir.join("stage"), dir.join("game"));
        write(&game.join("msimg32.dll"), b"old dll");
        write(&game.join("same.txt"), b"same");
        write(&stage.join("msimg32.dll"), b"new dll");
        write(&stage.join("same.txt"), b"same");
        write(&stage.join("GW2Translated/translator/t.exe"), b"new exe");
        let files: Vec<PathBuf> =
            ["same.txt", "GW2Translated/translator/t.exe", "msimg32.dll"].iter().map(PathBuf::from).collect();
        let done = install(&stage, &files, &game).unwrap();
        assert_eq!(done, Installed { replaced: 2, unchanged: 1, pending_old: vec![] });
        assert_eq!(fs::read(game.join("msimg32.dll")).unwrap(), b"new dll");
        assert_eq!(fs::read(game.join("GW2Translated/translator/t.exe")).unwrap(), b"new exe");
        assert_eq!(fs::read(game.join("same.txt")).unwrap(), b"same");
        assert!(!game.join("msimg32.dll.old").exists(), "an old copy not in use is deleted");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_install_is_rolled_back() {
        let dir = scratch("rollback");
        let (stage, game) = (dir.join("stage"), dir.join("game"));
        write(&game.join("a.dll"), b"old a");
        write(&game.join("c.dll"), b"old c");
        write(&stage.join("a.dll"), b"new a");
        write(&stage.join("sub/b.txt"), b"new b");
        // c.dll is listed but missing from the stage: the third step fails after c.dll was moved aside.
        let files: Vec<PathBuf> = ["a.dll", "sub/b.txt", "c.dll"].iter().map(PathBuf::from).collect();
        let err = install(&stage, &files, &game).unwrap_err();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(fs::read(game.join("a.dll")).unwrap(), b"old a");
        assert_eq!(fs::read(game.join("c.dll")).unwrap(), b"old c");
        assert!(!game.join("sub").exists(), "new files and folders are removed");
        for old in ["a.dll.old", "c.dll.old"] {
            assert!(!game.join(old).exists(), "{old} left behind");
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// The real case: the DLL being replaced is loaded (as our msimg32.dll is inside the game).
    #[test]
    fn a_loaded_dll_is_replaced_and_its_old_copy_swept_later() {
        use windows_sys::Win32::Foundation::FreeLibrary;
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
        let dir = scratch("loaded");
        let (stage, game) = (dir.join("stage"), dir.join("game"));
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        fs::create_dir_all(&game).unwrap();
        fs::copy(Path::new(&sys).join("System32").join("msimg32.dll"), game.join("msimg32.dll")).unwrap();
        let wide: Vec<u16> = game.join("msimg32.dll").to_string_lossy().encode_utf16().chain([0]).collect();
        let h = unsafe { LoadLibraryW(wide.as_ptr()) };
        assert!(!h.is_null());
        assert!(fs::remove_file(game.join("msimg32.dll")).is_err(), "a loaded DLL cannot be deleted");
        write(&stage.join("msimg32.dll"), b"new dll");
        let done = install(&stage, &[PathBuf::from("msimg32.dll")], &game).unwrap();
        assert_eq!(fs::read(game.join("msimg32.dll")).unwrap(), b"new dll");
        assert_eq!(done.pending_old, vec![game.join("msimg32.dll.old")], "still loaded: deleted at the next start");
        unsafe { FreeLibrary(h) };
        assert!(sweep_listed(&game, "msimg32.dll.old").is_empty());
        assert!(!game.join("msimg32.dll.old").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_old_copy_that_cannot_be_deleted_gets_another_name() {
        let dir = scratch("oldname");
        let dest = dir.join("x.dll");
        assert_eq!(free_old_path(&dest).unwrap(), dir.join("x.dll.old"));
        // A folder named like the .old file stands in for one in use (cannot be removed with remove_file).
        fs::create_dir_all(dir.join("x.dll.old")).unwrap();
        assert_eq!(free_old_path(&dest).unwrap(), dir.join("x.dll.1.old"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sweep_deletes_only_listed_old_files() {
        let dir = scratch("sweep");
        write(&dir.join("msimg32.dll.old"), b"x");
        write(&dir.join("GW2Translated/translator/t.exe.old"), b"x");
        write(&dir.join("keep.dll"), b"x");
        let left = sweep_listed(&dir, "msimg32.dll.old\nGW2Translated\\translator\\t.exe.old\nkeep.dll\n../x.old\n");
        assert!(left.is_empty());
        assert!(!dir.join("msimg32.dll.old").exists());
        assert!(!dir.join("GW2Translated/translator/t.exe.old").exists());
        assert!(dir.join("keep.dll").exists(), "only .old files are deleted");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unpack_refuses_unsafe_zips_and_skips_protected_files() {
        use zip::write::SimpleFileOptions;
        let dir = scratch("unpack");
        let make = |name: &str, entries: &[(&str, &[u8])]| {
            let p = dir.join(name);
            let mut w = zip::ZipWriter::new(fs::File::create(&p).unwrap());
            let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (n, d) in entries {
                w.start_file(*n, opts).unwrap();
                w.write_all(d).unwrap();
            }
            w.finish().unwrap();
            p
        };
        let good = make(
            "good.zip",
            &[
                ("msimg32.dll", b"dll"),
                ("GW2Translated/config.ini", b"user"),
                ("GW2Translated/translator/t.exe", b"exe"),
            ],
        );
        let files = unpack(&good, &dir.join("out")).unwrap();
        assert_eq!(
            files,
            vec![PathBuf::from("GW2Translated").join("translator").join("t.exe"), PathBuf::from("msimg32.dll")]
        );
        assert!(!dir.join("out/GW2Translated/config.ini").exists());
        let evil = make("evil.zip", &[("ok.txt", b"x"), ("../evil.dll", b"x")]);
        assert!(unpack(&evil, &dir.join("out2")).unwrap_err().contains("unsafe path"));
        assert!(!dir.join("evil.dll").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn release_json_gives_version_and_api_asset_urls() {
        let r = parse_release(
            r#"{"tag_name":"v1.2.0","assets":[
                {"name":"GW2Enhanced-All-1.2.0.zip","url":"https://api.github.com/x/1","size":42,
                 "browser_download_url":"https://github.com/x"},
                {"name":"SHA256SUMS.txt","url":"https://api.github.com/x/2"},
                {"name":"broken"}]}"#,
        )
        .unwrap();
        assert_eq!(r.version, "1.2.0");
        assert_eq!(r.assets.len(), 2);
        let a = r.asset("GW2Enhanced-All-1.2.0.zip").unwrap();
        assert_eq!((a.url.as_str(), a.size), ("https://api.github.com/x/1", 42));
        assert!(parse_release("{}").is_err());
        assert!(parse_release("not json").is_err());
    }
}
