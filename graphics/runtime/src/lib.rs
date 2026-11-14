//! GW2 Enhanced Graphics: `wtsapi32.dll` proxy for Guild Wars 2.
//!
//! `DllMain` only spawns the init thread (nothing under the loader lock).

pub mod config;
pub mod log;
#[cfg(feature = "standalone")]
mod proxy;
#[allow(dead_code)] // the log parser (`parse_frame_log`) is used by the tests only
mod passes;
#[cfg(test)]
mod frame_fixture;
mod classify;
mod ctxid;
mod diag;
mod d3d;
mod intercept;
pub mod overlay;
mod recorder;
mod shaders;
mod hdr_shaders;
mod upgrade;
mod gate;
mod resolve;
mod ao;
mod bloomfix;
mod ssr;
mod shafts;
mod aniso;
mod aa;
#[allow(dead_code)] // host mirrors of the shader maths are used by the tests
mod grading;
#[allow(dead_code)] // host mirrors of the shader maths are used by the tests
mod lens;
mod look;
mod fog;
mod contact;
mod aerial;
mod exposure;
mod furmask;
mod furpick;
mod shellfur;
mod furprobe;
mod furisland;
mod depthstats;
mod halo;
mod datapath;
mod drawdiag;
mod tonemap;
mod output;
mod hdr10;
mod calib;
mod photo;
mod devshot;
mod toggles;
mod clampemu;
#[cfg(test)]
mod testlock;

#[cfg(feature = "standalone")]
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "standalone")]
use windows_sys::Win32::Foundation::{HMODULE, TRUE};
#[cfg(feature = "standalone")]
use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
#[cfg(feature = "standalone")]
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

pub static MODULE: AtomicUsize = AtomicUsize::new(0);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn exe_path() -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Starts the graphics runtime. Call from a thread that is not under the loader lock.
pub fn init() {
    let Some(exe) = exe_path() else { return };
    if !exe.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("Gw2-64.exe")) {
        return;
    }
    let Some(dir) = exe.parent() else { return };
    let data = dir.join("GW2Graphics");
    if std::fs::create_dir_all(&data).is_err() {
        return;
    }
    log::init(&data.join("gw2graphics_log.txt"));
    let cfg = config::Config::load(&data.join("config.ini"));
    log!("==== GW2 Enhanced Graphics v{VERSION} ==== exe={}", exe.display());
    log!("config: {cfg:?}");
    upgrade::set_forced_off(!cfg.hdr_upgrade);
    clampemu::set_enabled(cfg.hdr_clamp);
    if !cfg.hdr_upgrade {
        log!("upgrade: OFF by hdr_upgrade=0 (diagnostic): the engine's own render-target formats, no substitute, no effect");
    }
    upgrade::set_enabled(cfg.enabled);
    gate::set_user_enabled(cfg.enabled);
    output::set_data_dir(&data);
    output::init(&cfg);
    photo::set_hotkey_text(&cfg.photo_key);
    output::set_sdr_gamma(cfg.sdr_gamma);
    output::apply_user_levels(cfg.enabled, cfg.paper_white_nits, cfg.peak_nits);
    toggles::apply(&cfg.toggles);
    hdr_shaders::set_bloom_strength(cfg.bloom_strength);
    resolve::set_highlight(cfg.highlight_boost);
    ao::init(cfg.ao_strength, cfg.ao_radius);
    halo::init(cfg.halo_strength, cfg.halo_radius);
    ssr::init(cfg.ssr_strength, cfg.ssr_range);
    ssr::set_classes(cfg.ssr_classes);
    ssr::set_smart(cfg.ssr_smart);
    ssr::set_hue_filter(cfg.ssr_hue_filter);
    ssr::set_flatness(cfg.ssr_flatness);
    ssr::set_water_boost(cfg.ssr_water_boost);
    ssr::set_depth_look(cfg.water_depth_look);
    ssr::set_matte_cut(cfg.ssr_matte_cut);
    shafts::init(cfg.shafts_strength, cfg.shafts_length);
    aniso::init(cfg.aniso_level, cfg.mip_bias);
    aa::init(cfg.smaa, cfg.smaa_threshold, cfg.sharpen_strength);
    lens::set_strength(cfg.lens_strength);
    lens::set_threshold(cfg.lens_threshold);
    lens::set_ghost(cfg.lens_ghost);
    lens::set_streak_length(cfg.lens_streak_length);
    grading::init(&data, cfg.grade(), &cfg.grade_lut, cfg.grade_lut_strength);
    look::init();
    datapath::set_fur(cfg.fur_strength);
    // The six parameters of the fur look; the diagnostic view (and debug_view) are runtime only: never applied at start.
    datapath::set_fur_look(&datapath::FurLook { view: 0, ..cfg.fur_look() });
    furmask::set_uv_pref(&cfg.fur_uv);
    furmask::set_occlusion(cfg.fur_occlusion != 0);
    datapath::set_rim(cfg.rim_strength);
    datapath::set_rim_width(cfg.rim_width);
    datapath::set_subsurface(cfg.subsurface_strength);
    furmask::init(&cfg.fur_ps,&cfg.fur_vs, cfg.fur_min_vcb, cfg.fur_debug);
    furmask::configure_rim(&cfg.rim_ps, cfg.rim_min_vcb, cfg.rim_debug);
    shellfur::init(&cfg.shell);
    fog::init();
    contact::init(cfg.contact_strength, cfg.contact_length, &cfg.contact_sun);
    aerial::init(cfg.aerial_strength, cfg.aerial_distance, cfg.depth_unit);
    exposure::init(cfg.autoexp_strength, cfg.autoexp_target, cfg.autoexp_range, cfg.adapt_up, cfg.adapt_down, cfg.localtone_strength);
    hdr_shaders::install_all();
    recorder::set_dump_dir(data.clone());
    if !data.join("config.ini").exists() {
        let _ = cfg.save(&data.join("config.ini"));
    }
    // Heartbeat: reports a stall of the resolved frames (a hung game) with the time it lasted.
    let _ = std::thread::Builder::new().name("gw2graphics-watchdog".into()).spawn(|| {
        let (mut last, mut since, mut reported) = (0u64, std::time::Instant::now(), false);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let n = resolve::frames_resolved();
            if n != last {
                if reported {
                    log!("watchdog: frames resumed after {:.1} s", since.elapsed().as_secs_f32());
                }
                (last, since, reported) = (n, std::time::Instant::now(), false);
            } else if n > 0 && !reported && since.elapsed().as_secs_f32() >= 2.0 {
                reported = true;
                log!("watchdog: no resolved frame for 2 s (frames {n}, overlay {}, gate {})", overlay::visible(), recorder::gate_on());
            }
        }
    });
    if d3d::install() {
        // hudhook's Present hook is the frame tick. Installed late, like the Remastered overlay:
        // earlier, the device and the other mods' hooks are not up yet.
        let path = data.join("config.ini");
        let _ = std::thread::Builder::new().name("gw2graphics-overlay".into()).spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(8));
            let _ = std::panic::catch_unwind(move || overlay::install(MODULE.load(Ordering::Relaxed), cfg, path));
        });
    } else {
        log!("d3d: no hooks installed; nothing to record");
    }
}

#[cfg(feature = "standalone")]
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module as usize, Ordering::Relaxed);
        unsafe { DisableThreadLibraryCalls(module) };
        let _ = std::thread::Builder::new().name("gw2graphics-init".into()).spawn(|| {
            let _ = std::panic::catch_unwind(init);
        });
    }
    TRUE
}
