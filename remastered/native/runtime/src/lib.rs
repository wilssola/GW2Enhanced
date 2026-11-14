//! GW2 Remastered: `msimg32.dll` proxy that serves HD textures to Guild Wars 2.
//!
//! Coexists with GW2 Translated (`version.dll`) and ReShade (`dxgi.dll`/`d3d11.dll`):
//! different proxy name; texture hooks only on the game's own file layer. The
//! overlay (Shift+Home) uses hudhook on Present like GW2 Translated does. `DllMain` only spawns the init thread (nothing under the loader lock).

pub mod mem;
mod atex;
mod composite;
mod diag;
mod fallback;
mod game;
pub mod overlay;
mod player_diag;
pub mod log;
#[cfg(feature = "standalone")]
mod proxy;
mod reload;
mod resize;
mod sig;

#[cfg(feature = "standalone")]
use std::ffi::c_void;
use std::path::PathBuf;

#[cfg(feature = "standalone")]
use windows_sys::Win32::Foundation::{HMODULE, TRUE};
#[cfg(feature = "standalone")]
use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
#[cfg(feature = "standalone")]
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Lets installers recognise our `msimg32.dll`.
#[used]
static DLL_MARKER: [u8; 24] = *b"GW2REMASTERED-DLL-MARKER";

fn exe_path() -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

pub static MODULE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "standalone")]
#[global_allocator]
static ALLOC: mem::CountingAlloc = mem::CountingAlloc;

/// Starts the texture runtime. Call from a thread that is not under the loader lock.
pub fn init() {
    let Some(exe) = exe_path() else { return };
    if !exe.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("Gw2-64.exe")) {
        return;
    }
    let Some(dir) = exe.parent() else { return };
    let data = dir.join("GW2Remastered");
    if std::fs::create_dir_all(&data).is_err() {
        return;
    }
    log::init(&data.join("gw2remastered_log.txt"));
    log!("==== GW2 Remastered v{VERSION} ==== exe={}", exe.display());
    match game::Image::current() {
        Some(img) => {
            log!("game image at {:#x}", img.base);
            overlay::load_settings(data.join("config.ini"));
            diag::start(&img, &data);
            diag::guard_start();
            reload::install(&img);
            player_diag::install(&img);
            // Same moment the overlay always hooked at (the pack load used to delay it);
            // this early the device and the other mods' hooks are not up yet.
            let _ = std::thread::Builder::new().name("gw2remastered-overlay".into()).spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(8));
                let _ = std::panic::catch_unwind(|| overlay::install(MODULE.load(std::sync::atomic::Ordering::Relaxed)));
            });
        }
        None => log!("game image not found; nothing to do"),
    }
}

#[cfg(feature = "standalone")]
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe { DisableThreadLibraryCalls(module) };
        MODULE.store(module as usize, std::sync::atomic::Ordering::Relaxed);
        let _ = std::thread::Builder::new().name("gw2remastered-init".into()).spawn(|| {
            let _ = std::panic::catch_unwind(init);
        });
    }
    TRUE
}

/// (level, free commit in MB) for the host's Health section: 0 fine, 1 low, 2 critical.
pub fn commit_status() -> (u8, u64) {
    (
        diag::COMMIT_LEVEL.load(std::sync::atomic::Ordering::Relaxed),
        diag::FREE_COMMIT_MB.load(std::sync::atomic::Ordering::Relaxed),
    )
}
