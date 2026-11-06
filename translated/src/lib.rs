//! GW2 Translated: a `version.dll` proxy that translates Guild Wars 2.
//!
//! `DllMain` only schedules initialization on its own thread; loading
//! files or installing hooks under the loader lock can hang the game.

mod bubble;
pub mod chat;
pub mod coded;
pub mod collect;
pub mod config;
pub mod dict;
pub mod game;
pub mod gpu;
pub mod hooks;
pub mod layout;
pub mod live;
pub mod log;
mod net;
mod overlay;
pub mod paths;
pub mod procstats;
mod proxy;
pub mod rc4;
pub mod records;
pub mod refresh;
pub mod runtime;
pub mod sig;
mod ui_text;
mod upload;
mod web;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{HMODULE, TRUE};
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

use config::Config;
use dict::Dict;
use paths::Paths;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Lets the installer recognize our `version.dll` among other mods.
#[used]
static DLL_MARKER: [u8; 24] = *b"GW2TRANSLATED-DLL-MARKER";

/// This DLL's module handle (the overlay needs it).
static MODULE: AtomicUsize = AtomicUsize::new(0);

/// Global state, filled in exactly once by the initialization thread.
pub struct State {
    pub paths: Paths,
    pub config: Config,
    pub dict: Dict,
    /// Collection thread; whether it records anything follows [`runtime`].
    pub collector: Option<collect::Collector>,
}

pub static STATE: OnceLock<State> = OnceLock::new();

fn exe_path() -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

fn init() {
    let Some(exe) = exe_path() else { return };
    // Other processes in the folder (tools, crash reporter) only get the forwarding.
    let is_game = exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Gw2-64.exe"));
    if !is_game {
        return;
    }
    let Some(game_dir) = exe.parent() else { return };
    let base = Paths::new(game_dir, "pt-BR");
    if std::fs::create_dir_all(&base.data).is_err() {
        return;
    }
    log::init(&base.log());
    log!("==== GW2 Translated v{VERSION} ==== exe={}", exe.display());

    let config = match Config::load_or_create(&base.config()) {
        Ok(c) => c,
        Err(e) => {
            log!(
                "config: error reading {} ({e}); using defaults",
                base.config().display()
            );
            Config::default()
        }
    };
    log!("config: {config:?}");
    let paths = Paths::new(game_dir, &config.language);
    if !paths.language_dir().is_dir() {
        log!(
            "language {}: folder {} does not exist",
            config.language,
            paths.language_dir().display()
        );
    }

    runtime::init(&config, paths.config());
    let mut dict = Dict::default();
    {
        let t = std::time::Instant::now();
        match dict.load_file(&paths.dictionary(), true) {
            Ok(n) => log!("dictionary: {n} texts"),
            Err(e) => log!("dictionary: error {e}"),
        }
        match dict.load_file(&paths.local(), false) {
            Ok(n) => log!("local translations: {n} new texts"),
            Err(e) => log!("local translations: error {e}"),
        }
        log!(
            "dictionary loaded in {:.2}s ({} pairs, {} ids)",
            t.elapsed().as_secs_f64(),
            dict.len(),
            dict.id_len()
        );
    }

    // Always started: the overlay can turn translation and collection on and
    // off while the game runs. The local translator only starts when needed.
    let collector = collect::start(paths.discovered(), paths.decrypted())
        .inspect_err(|e| log!("collection: could not start: {e}"))
        .ok();
    let _ = STATE.set(State {
        paths,
        config,
        dict,
        collector,
    });

    if let Some(st) = STATE.get() {
        let mut known = Dict::default();
        let _ = known.load_file(&st.paths.local(), true);
        live::start(live::Setup {
            exe: st.paths.translator_exe(),
            model: st.paths.model_dir(),
            model_big: st.paths.model_big_dir(),
            gpu_runtime: st.paths.gpu_runtime_dir(),
            port_file: st.paths.translator_dir().join("port.txt"),
            local_file: st.paths.local(),
            web_file: st.paths.web_local(),
            known: known.into_map(),
            known_web: records::read::<records::Translation>(&st.paths.web_local())
                .map(|r| r.records.into_iter().map(|t| t.en).collect())
                .unwrap_or_default(),
        });
        // Sends nothing unless the player agreed (checked on every cycle).
        upload::start(upload::Setup {
            state_file: st.paths.data.join("upload_state.json"),
            files: vec![st.paths.decrypted(), st.paths.local(), st.paths.discovered()],
            language: st.paths.language.clone(),
        });
    }

    match game::Image::current() {
        Some(img) => {
            hooks::install(&img, true);
            refresh::install(&img);
            chat::install(&img);
            bubble::install(&img);
        }
        None => log!("game image not found; nothing to do"),
    }
    if let Some(st) = STATE.get() {
        web::install(st.paths.language_dir().join("web.jsonl"), st.paths.web_local(), &st.dict);
    }
    if let Some(st) = STATE.get().filter(|s| s.config.overlay) {
        overlay::install(
            MODULE.load(Ordering::Relaxed),
            &st.config.language,
            st.paths.data.join("languages"),
            st.config.overlay_key,
        );
    }
    log!("initialization complete");
}

/// DLL entry point.
///
/// # Safety
///
/// Called only by the Windows loader, with a valid module handle.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe { DisableThreadLibraryCalls(module) };
        MODULE.store(module as usize, Ordering::Relaxed);
        // The thread only starts running after the loader lock is released.
        let _ = std::thread::Builder::new()
            .name("gw2translated-init".into())
            .spawn(init);
    }
    TRUE
}
