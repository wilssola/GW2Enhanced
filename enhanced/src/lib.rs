//! GW2 Enhanced: one `msimg32.dll` that hosts every feature (translation, HD textures, graphics).
//!
//! `DllMain` only spawns a thread (nothing runs under the loader lock). Each feature starts inside its own
//! panic guard, so one that fails never stops the others. The features' panels share one overlay (one hook
//! on `Present`; the game cannot have several).

mod proxy;

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hudhook::hooks::dx11::ImguiDx11Hooks;
use hudhook::imgui::{Condition, Context, Io, Ui, WindowFlags};
use hudhook::windows::Win32::Foundation::HINSTANCE;
use hudhook::{Hudhook, ImguiRenderLoop, MessageFilter, RenderContext};
use windows_sys::Win32::Foundation::{HMODULE, TRUE};
use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

use gw2enhanced_core::registry::{self, State};

/// Lets the installer recognise our `msimg32.dll`.
#[used]
#[unsafe(no_mangle)]
pub static GW2ENHANCED_DLL_MARKER: [u8; 24] = *b"GW2ENHANCED-DLL-MARKER!!";

// The texture runtime counts the bytes the mod holds (shown in its panel).
#[global_allocator]
static ALLOC: gw2remastered::mem::CountingAlloc = gw2remastered::mem::CountingAlloc;

/// Runs `f` as feature `name`; a panic turns the feature off and is recorded instead of propagating.
fn guarded(name: &'static str, f: impl FnOnce() + std::panic::UnwindSafe) {
    match std::panic::catch_unwind(f) {
        Ok(()) => {
            if registry::feature_state(name).is_none() {
                registry::report(name, "", State::Ok);
            }
        }
        Err(_) => registry::report(name, "", State::Off("panicked while starting".into())),
    }
}

fn init(module: usize) {
    gw2remastered::MODULE.store(module, Ordering::Relaxed);
    gw2translated::MODULE.store(module, Ordering::Relaxed);
    gw2graphics::MODULE.store(module, Ordering::Relaxed);
    // Each feature has its own panic guard and thread: they hook at different moments and one must not wait for another.
    let t = std::thread::Builder::new()
        .name("gw2enhanced-translated".into())
        .spawn(|| guarded("translation", gw2translated::init));
    let r = std::thread::Builder::new()
        .name("gw2enhanced-textures".into())
        .spawn(|| guarded("textures", gw2remastered::init));
    let g = std::thread::Builder::new()
        .name("gw2enhanced-graphics".into())
        .spawn(|| guarded("graphics", gw2graphics::init));
    let o = std::thread::Builder::new().name("gw2enhanced-overlay".into()).spawn(move || {
        guarded("overlay", move || install_overlay(module));
    });
    // Health: the system commit (RAM + pagefile) is what ran out in the game's "OOM: Heap" crash.
    let m = std::thread::Builder::new().name("gw2enhanced-memory".into()).spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            let (level, mb) = gw2remastered::commit_status();
            let state = match level {
                0 => State::Ok,
                1 => State::Degraded(format!("free commit {mb} MB: caches trimmed (raise the pagefile)")),
                _ => State::Degraded(format!("free commit {mb} MB: HD loading paused until memory is free (raise the pagefile)")),
            };
            registry::report("memory", "commit", state);
        }
    });
    let _ = m;
    for h in [t, r, g, o].into_iter().flatten() {
        let _ = h.join();
    }
}

/// The panels of the features that have one, drawn by a single render loop.
struct Overlay {
    translated: Option<gw2translated::overlay::Overlay>,
    textures: Option<gw2remastered::overlay::Overlay>,
    graphics: Option<gw2graphics::overlay::Overlay>,
}

impl ImguiRenderLoop for Overlay {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, rc: &'a mut dyn RenderContext) {
        if let Some(o) = self.translated.as_mut() {
            o.initialize(ctx, rc);
        }
        if let Some(o) = self.textures.as_mut() {
            o.initialize(ctx, rc);
        }
        if let Some(o) = self.graphics.as_mut() {
            o.initialize(ctx, rc);
        }
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, rc: &'a mut dyn RenderContext) {
        if let Some(o) = self.translated.as_mut() {
            o.before_render(ctx, rc);
        }
        if let Some(o) = self.textures.as_mut() {
            o.before_render(ctx, rc);
        }
        if let Some(o) = self.graphics.as_mut() {
            o.before_render(ctx, rc);
        }
        // Each panel sets the cursor for itself; the host draws it when any is open (never under HDR10: it would be 10000 nits).
        ctx.io_mut().mouse_draw_cursor = (gw2translated::overlay::is_visible() || gw2remastered::overlay::is_visible())
            && !gw2graphics::overlay::hdr10_active();
    }

    fn render(&mut self, ui: &mut Ui) {
        // One hotkey for everything: the texture panel's (Shift+Home by default) owns it.
        if let Some(o) = self.textures.as_mut() {
            o.tick();
        } else if let Some(o) = self.translated.as_mut() {
            o.poll_key();
        }
        // The translation panel asked to close (or opens its first-time setup, a window of its own).
        if gw2translated::overlay::HIDE_HOST.swap(false, Ordering::Relaxed) {
            gw2remastered::overlay::set_visible(false);
        }
        if let Some(o) = self.translated.as_mut()
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.wizard(ui))).is_err()
        {
            self.translated = None;
            registry::report("translation", "overlay", State::Off("setup window panicked".into()));
        }
        let shown = gw2remastered::overlay::is_visible();
        // Graphics works every frame (it is the frame tick of its passes), open panel or not.
        if let Some(o) = self.graphics.as_mut() {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                o.per_frame();
                o.after_frame(ui, shown, |_, _| {});
            }));
            if r.is_err() {
                self.graphics = None;
                registry::report("graphics", "overlay", State::Off("frame work panicked".into()));
            }
        }
        if !shown {
            return;
        }
        let mut open = true;
        ui.window("GW2 Enhanced###gw2enhanced")
            .size([460.0, 0.0], Condition::FirstUseEver)
            .position([60.0, 60.0], Condition::FirstUseEver)
            .opened(&mut open)
            .flags(WindowFlags::NO_COLLAPSE | WindowFlags::ALWAYS_AUTO_RESIZE)
            .build(|| {
                if let Some(_bar) = ui.tab_bar("gw2enhanced-tabs") {
                    if let Some(o) = self.translated.as_mut()
                        && let Some(_t) = ui.tab_item("Translation")
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.body(ui))).is_err()
                    {
                        registry::report("translation", "overlay", State::Off("panel panicked".into()));
                    }
                    if let Some(o) = self.textures.as_mut()
                        && let Some(_t) = ui.tab_item("Textures")
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.body(ui))).is_err()
                    {
                        registry::report("textures", "overlay", State::Off("panel panicked".into()));
                    }
                    if let Some(o) = self.graphics.as_mut()
                        && let Some(_t) = ui.tab_item("Graphics")
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.body(ui))).is_err()
                    {
                        registry::report("graphics", "overlay", State::Off("panel panicked".into()));
                    }
                    if let Some(_t) = ui.tab_item("Health") {
                        health(ui);
                    }
                }
            });
        if !open {
            gw2remastered::overlay::set_visible(false);
        }
    }

    fn message_filter(&self, io: &Io) -> MessageFilter {
        let a = self.translated.as_ref().map_or(MessageFilter::empty(), |o| o.message_filter(io));
        let a = a | self.graphics.as_ref().map_or(MessageFilter::empty(), |o| o.message_filter(io));
        let b = self.textures.as_ref().map_or(MessageFilter::empty(), |o| o.message_filter(io));
        a | b
    }
}

/// Every feature and hook with its state, worst first: what the player needs to see after a game update.
fn health(ui: &Ui) {
    let mut all = registry::snapshot();
    let rank = |s: &State| match s {
        State::Off(_) => 0,
        State::Degraded(_) => 1,
        State::Ok => 2,
    };
    all.sort_by_key(|e| (rank(&e.state), e.feature));
    let off = all.iter().filter(|e| e.state.is_off()).count();
    let degraded = all.iter().filter(|e| matches!(e.state, State::Degraded(_))).count();
    if off == 0 && degraded == 0 {
        ui.text_colored([0.4, 1.0, 0.4, 1.0], "Everything is working.");
    } else {
        ui.text_colored([1.0, 0.7, 0.3, 1.0], format!("{off} off, {degraded} degraded"));
    }
    ui.separator();
    for e in &all {
        let what = if e.item.is_empty() { e.feature.to_string() } else { format!("{} / {}", e.feature, e.item) };
        match &e.state {
            State::Ok => ui.text_colored([0.4, 1.0, 0.4, 1.0], format!("OK        {what}")),
            State::Degraded(t) => ui.text_colored([1.0, 0.85, 0.3, 1.0], format!("DEGRADED  {what}: {t}")),
            State::Off(t) => ui.text_colored([1.0, 0.4, 0.4, 1.0], format!("OFF       {what}: {t}")),
        }
    }
    ui.separator();
    ui.text_disabled(format!("GW2 Enhanced v{}", env!("CARGO_PKG_VERSION")));
}

/// Waits for the features to publish their panels (they do it at different moments), then hooks `Present` once.
fn install_overlay(module: usize) {
    let start = Instant::now();
    let (mut translated, mut textures, mut graphics) = (None, None, None);
    while start.elapsed() < Duration::from_secs(40) {
        translated = translated.or_else(|| gw2translated::overlay::PANEL.lock().unwrap_or_else(|e| e.into_inner()).take());
        textures = textures.or_else(|| gw2remastered::overlay::PANEL.lock().unwrap_or_else(|e| e.into_inner()).take());
        graphics = graphics.or_else(|| gw2graphics::overlay::PANEL.lock().unwrap_or_else(|e| e.into_inner()).take());
        if translated.is_some() && textures.is_some() && graphics.is_some() && start.elapsed() > Duration::from_secs(8) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if translated.is_none() && textures.is_none() && graphics.is_none() {
        registry::report("overlay", "", State::Off("no feature published a panel".into()));
        return;
    }
    let hooks = Hudhook::builder()
        .with::<ImguiDx11Hooks>(Overlay { translated, textures, graphics })
        .with_hmodule(HINSTANCE(module as *mut _))
        .build();
    if let Err(e) = gw2graphics::overlay::while_hooking_present(|| hooks.apply()) {
        registry::report("overlay", "", State::Off(format!("could not hook Present: {e:?}")));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe { DisableThreadLibraryCalls(module) };
        let m = module as usize;
        let _ = std::thread::Builder::new()
            .name("gw2enhanced-init".into())
            .spawn(move || init(m));
    }
    TRUE
}
