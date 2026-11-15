//! GW2 Enhanced: one `msimg32.dll` that hosts every feature (translation, HD textures, graphics).
//!
//! Which features are inside is chosen at build time (cargo features `translation`, `textures`, `graphics`, all on
//! by default): each project builds the same DLL, and building all of them gives one DLL with the three mods.
//!
//! `DllMain` only spawns a thread (nothing runs under the loader lock). Each feature starts inside its own
//! panic guard, so one that fails never stops the others. The features' panels share one overlay (one hook
//! on `Present`; the game cannot have several).

mod proxy;
mod update;

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

use gw2enhanced_core::i18n::{self, Lang, tr};
use gw2enhanced_core::logs::{self, Feature};
use gw2enhanced_core::registry::{self, State};

/// Lets the installer recognise our `msimg32.dll`.
#[used]
#[unsafe(no_mangle)]
pub static GW2ENHANCED_DLL_MARKER: [u8; 24] = *b"GW2ENHANCED-DLL-MARKER!!";

/// Lets the installer show which version and package this DLL is (read from the file, never loaded).
#[used]
#[unsafe(no_mangle)]
pub static GW2ENHANCED_BUILD_INFO: [u8; gw2enhanced_core::release::BUILD_INFO_LEN] =
    gw2enhanced_core::release::build_info(
        match option_env!("GW2E_VERSION") {
            Some(v) => v,
            None => env!("CARGO_PKG_VERSION"),
        },
        match option_env!("GW2E_PACKAGE") {
            Some(p) => p,
            None => "",
        },
    );

// The texture runtime counts the bytes the mod holds (shown in its panel).
#[cfg(feature = "textures")]
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
        Err(_) => {
            logs::HOST.write(&format!("{name}: panicked while starting"));
            registry::report(name, "", State::Off("panicked while starting".into()));
        }
    }
}

fn init(module: usize) {
    // Before any panel exists: they all draw in the overlay language.
    i18n::init();
    // Off unless log_host=1 (the features open their own logs the same way).
    if let Some(dir) = gw2enhanced_core::settings::path().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        logs::HOST.init(&dir.join("gw2enhanced_log.txt"));
    }
    logs::HOST.write(&format!("GW2 Enhanced {} starting", update::current_version()));
    let mut threads = Vec::new();
    // Each feature has its own panic guard and thread: they hook at different moments and one must not wait for another.
    #[cfg(feature = "translation")]
    {
        gw2translated::MODULE.store(module, Ordering::Relaxed);
        threads.push(
            std::thread::Builder::new()
                .name("gw2enhanced-translated".into())
                .spawn(|| guarded("translation", gw2translated::init)),
        );
    }
    #[cfg(feature = "textures")]
    {
        gw2remastered::MODULE.store(module, Ordering::Relaxed);
        threads.push(
            std::thread::Builder::new()
                .name("gw2enhanced-textures".into())
                .spawn(|| guarded("textures", gw2remastered::init)),
        );
    }
    #[cfg(feature = "graphics")]
    {
        gw2graphics::MODULE.store(module, Ordering::Relaxed);
        threads.push(
            std::thread::Builder::new()
                .name("gw2enhanced-graphics".into())
                .spawn(|| guarded("graphics", gw2graphics::init)),
        );
    }
    threads.push(
        std::thread::Builder::new()
            .name("gw2enhanced-overlay".into())
            .spawn(move || {
                guarded("overlay", move || install_overlay(module));
            }),
    );
    // Health: the system commit (RAM + pagefile) is what ran out in the game's "OOM: Heap" crash.
    #[cfg(feature = "textures")]
    let _ = std::thread::Builder::new().name("gw2enhanced-memory".into()).spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            let (level, mb) = gw2remastered::commit_status();
            let state = match level {
                0 => State::Ok,
                1 => State::Degraded(format!("free commit {mb} MB: caches trimmed (raise the pagefile)")),
                _ => State::Degraded(format!(
                    "free commit {mb} MB: HD loading paused until memory is free (raise the pagefile)"
                )),
            };
            registry::report("memory", "commit", state);
        }
    });
    // Built in whatever mods are; it waits a few seconds on its own thread and never blocks the game.
    update::start();
    for h in threads.into_iter().flatten() {
        let _ = h.join();
    }
}

// The window's open / closed state and its hotkey belong to one feature: textures (Shift+Home, changeable in its
// panel) when it is built in, else translation, else graphics.

/// Reads the hotkey (and the first-run prompts) of the feature that owns the window.
fn host_tick(o: &mut Overlay) {
    #[cfg(feature = "textures")]
    if let Some(p) = o.textures.as_mut() {
        p.tick();
        return;
    }
    #[cfg(feature = "translation")]
    if let Some(p) = o.translated.as_mut() {
        p.poll_key();
        return;
    }
    #[cfg(feature = "graphics")]
    if let Some(p) = o.graphics.as_mut() {
        p.poll_key();
    }
    let _ = o;
}

fn host_shown() -> bool {
    #[cfg(feature = "textures")]
    return gw2remastered::overlay::is_visible();
    #[cfg(all(not(feature = "textures"), feature = "translation"))]
    return gw2translated::overlay::is_visible();
    #[cfg(all(not(feature = "textures"), not(feature = "translation"), feature = "graphics"))]
    return gw2graphics::overlay::visible();
    #[allow(unreachable_code)]
    false
}

fn host_hide() {
    #[cfg(feature = "textures")]
    gw2remastered::overlay::set_visible(false);
    #[cfg(all(not(feature = "textures"), feature = "translation"))]
    gw2translated::overlay::set_visible(false);
    #[cfg(all(not(feature = "textures"), not(feature = "translation"), feature = "graphics"))]
    gw2graphics::overlay::set_visible(false);
}

/// Some window of ours is on screen (the main one, or the translation's first-time setup).
fn any_window() -> bool {
    #[cfg(feature = "translation")]
    if gw2translated::overlay::is_visible() {
        return true;
    }
    host_shown()
}

fn hdr10_active() -> bool {
    #[cfg(feature = "graphics")]
    return gw2graphics::overlay::hdr10_active();
    #[allow(unreachable_code)]
    false
}

/// The panels of the features that have one, drawn by a single render loop.
struct Overlay {
    #[cfg(feature = "translation")]
    translated: Option<gw2translated::overlay::Overlay>,
    #[cfg(feature = "textures")]
    textures: Option<gw2remastered::overlay::Overlay>,
    #[cfg(feature = "graphics")]
    graphics: Option<gw2graphics::overlay::Overlay>,
}

impl ImguiRenderLoop for Overlay {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, rc: &'a mut dyn RenderContext) {
        #[cfg(feature = "translation")]
        if let Some(o) = self.translated.as_mut() {
            o.initialize(ctx, rc);
        }
        #[cfg(feature = "textures")]
        if let Some(o) = self.textures.as_mut() {
            o.initialize(ctx, rc);
        }
        #[cfg(feature = "graphics")]
        if let Some(o) = self.graphics.as_mut() {
            o.initialize(ctx, rc);
        }
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, rc: &'a mut dyn RenderContext) {
        #[cfg(feature = "translation")]
        if let Some(o) = self.translated.as_mut() {
            o.before_render(ctx, rc);
        }
        #[cfg(feature = "textures")]
        if let Some(o) = self.textures.as_mut() {
            o.before_render(ctx, rc);
        }
        #[cfg(feature = "graphics")]
        if let Some(o) = self.graphics.as_mut() {
            o.before_render(ctx, rc);
        }
        let _ = rc;
        // Each panel sets the cursor for itself; the host draws it when any is open (never under HDR10: it would be 10000 nits).
        ctx.io_mut().mouse_draw_cursor = (any_window() || update::window_visible()) && !hdr10_active();
    }

    fn render(&mut self, ui: &mut Ui) {
        // One hotkey for everything (see host_tick).
        host_tick(self);
        // The translation panel asked to close (or opens its first-time setup, a window of its own).
        #[cfg(feature = "translation")]
        if gw2translated::overlay::HIDE_HOST.swap(false, Ordering::Relaxed) {
            host_hide();
        }
        #[cfg(feature = "translation")]
        if let Some(o) = self.translated.as_mut()
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.wizard(ui))).is_err()
        {
            self.translated = None;
            registry::report("translation", "overlay", State::Off("setup window panicked".into()));
        }
        let shown = host_shown();
        // Graphics works every frame (it is the frame tick of its passes), open panel or not.
        #[cfg(feature = "graphics")]
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
        // The update prompt is a window of its own, shown with the main panel closed too.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| update::draw(ui))).is_err() {
            registry::report("update", "overlay", State::Off("update window panicked".into()));
        }
        if !shown {
            return;
        }
        let mut open = true;
        // A fixed width and a height that fits the content: with ALWAYS_AUTO_RESIZE, panels whose wrapped text and
        // full-width widgets follow the window width made the window shrink a little every frame after a tab switch.
        let [dw, dh] = ui.io().display_size;
        ui.window(format!("GW2 Enhanced {}###gw2enhanced", gw2enhanced_core::release::label()))
            .size([window_width(dw, dh), 0.0], Condition::Always)
            .position([60.0, 60.0], Condition::FirstUseEver)
            .opened(&mut open)
            .flags(WindowFlags::NO_COLLAPSE)
            .build(|| {
                language_picker(ui);
                if let Some(_bar) = ui.tab_bar("gw2enhanced-tabs") {
                    #[cfg(feature = "translation")]
                    if let Some(o) = self.translated.as_mut()
                        && let Some(_t) = ui.tab_item(tr("Translation###tab-translation", "Tradução###tab-translation"))
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.body(ui))).is_err()
                    {
                        registry::report("translation", "overlay", State::Off("panel panicked".into()));
                    }
                    #[cfg(feature = "textures")]
                    if let Some(o) = self.textures.as_mut()
                        && let Some(_t) = ui.tab_item(tr("Textures###tab-textures", "Texturas###tab-textures"))
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.body(ui))).is_err()
                    {
                        registry::report("textures", "overlay", State::Off("panel panicked".into()));
                    }
                    #[cfg(feature = "graphics")]
                    if let Some(o) = self.graphics.as_mut()
                        && let Some(_t) = ui.tab_item(tr("Graphics###tab-graphics", "Gráficos###tab-graphics"))
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.body(ui))).is_err()
                    {
                        registry::report("graphics", "overlay", State::Off("panel panicked".into()));
                    }
                    if let Some(_t) = ui.tab_item(tr("Version###tab-version", "Versão###tab-version")) {
                        version_tab(ui);
                    }
                    if let Some(_t) = ui.tab_item(tr("Health###tab-health", "Situação###tab-health")) {
                        health(ui);
                    }
                }
            });
        if !open {
            host_hide();
        }
    }

    fn message_filter(&self, io: &Io) -> MessageFilter {
        let mut f = if host_shown() || update::window_visible() {
            MessageFilter::InputAll
        } else {
            MessageFilter::empty()
        };
        #[cfg(feature = "translation")]
        {
            f |= self
                .translated
                .as_ref()
                .map_or(MessageFilter::empty(), |o| o.message_filter(io));
        }
        #[cfg(feature = "textures")]
        {
            f |= self
                .textures
                .as_ref()
                .map_or(MessageFilter::empty(), |o| o.message_filter(io));
        }
        #[cfg(feature = "graphics")]
        {
            f |= self
                .graphics
                .as_ref()
                .map_or(MessageFilter::empty(), |o| o.message_filter(io));
        }
        let _ = io;
        f
    }
}

/// Width of the main window: 480 px at 1080p, scaled like the font (up to 2x), never wider than the screen.
fn window_width(display_w: f32, display_h: f32) -> f32 {
    let w = 480.0 * (display_h / 1080.0).clamp(1.0, 2.0);
    if display_w.is_finite() && display_w > 0.0 { w.min(display_w * 0.9) } else { w }
}

/// The overlay language, above the tabs so it is there whichever feature is built in.
fn language_picker(ui: &Ui) {
    let cur = i18n::current();
    ui.text(tr("Overlay language", "Idioma do overlay"));
    ui.same_line();
    ui.set_next_item_width(200.0 * ui.io().font_global_scale);
    if let Some(_c) = ui.begin_combo("##overlay-language", cur.name()) {
        for l in Lang::ALL {
            if ui.selectable_config(l.name()).selected(l == cur).build() && l != cur
                && let Err(e) = i18n::choose(l)
            {
                registry::report("overlay", "language", State::Degraded(format!("could not save ui_language: {e}")));
            }
        }
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
        ui.text_colored([0.4, 1.0, 0.4, 1.0], tr("Everything is working.", "Tudo funcionando."));
    } else {
        ui.text_colored(
            [1.0, 0.7, 0.3, 1.0],
            gw2enhanced_core::trf!("{off} off, {degraded} degraded", "{off} desligado(s), {degraded} parcial(is)"),
        );
    }
    ui.separator();
    for e in &all {
        let what = if e.item.is_empty() {
            e.feature.to_string()
        } else {
            format!("{} / {}", e.feature, e.item)
        };
        // The reasons come from the features as they report them (English, like the log).
        let (ok, degraded, off) = ("OK", tr("DEGRADED", "PARCIAL"), tr("OFF", "DESLIGADO"));
        let (color, line) = match &e.state {
            State::Ok => ([0.4, 1.0, 0.4, 1.0], format!("{ok:<10}{what}")),
            State::Degraded(t) => ([1.0, 0.85, 0.3, 1.0], format!("{degraded:<10}{what}: {t}")),
            State::Off(t) => ([1.0, 0.4, 0.4, 1.0], format!("{off:<10}{what}: {t}")),
        };
        // Wrapped: the window has a fixed width and the reasons can be long.
        let _c = ui.push_style_color(hudhook::imgui::StyleColor::Text, color);
        ui.text_wrapped(line);
    }
    ui.separator();
    log_switches(ui);
}

/// Version tab: what this DLL is, its update state, the manual check and the changelog of the newest release. One page
/// for the whole build: the full build updates GW2 Enhanced as a whole, a single-feature build only that feature.
fn version_tab(ui: &Ui) {
    ui.text(gw2enhanced_core::trf!("{}: version {}", "{}: versão {}", update_target(), update::current_version()));
    let names: Vec<&str> = log_features().into_iter().skip(1).map(|(_, n)| n).collect();
    if names.len() > 1 {
        dim_text(ui, &gw2enhanced_core::trf!("Included: {}", "Inclui: {}", names.join(", ")));
    }
    ui.separator();
    dim_text(ui, &update::summary());
    if ui.button(gw2enhanced_core::trf!("Check for updates: {}###check-updates", "Verificar atualizações: {}###check-updates", update_target()))
        && !update::checking()
    {
        update::check_now();
    }
    let (version, notes) = update::release_notes();
    if !notes.is_empty() {
        ui.separator();
        ui.text(gw2enhanced_core::trf!("Changelog of {version}", "Novidades da {version}"));
        ui.child_window("##version-notes").size([0.0, 260.0]).border(true).build(|| ui.text_wrapped(&notes));
    }
}

fn dim_text(ui: &Ui, text: &str) {
    let c = ui.style_color(hudhook::imgui::StyleColor::TextDisabled);
    let _c = ui.push_style_color(hudhook::imgui::StyleColor::Text, c);
    ui.text_wrapped(text);
}

/// What this build's updater updates: GW2 Enhanced in the full build, the one feature in a single-feature build.
fn update_target() -> &'static str {
    let n = cfg!(feature = "translation") as u8 + cfg!(feature = "textures") as u8 + cfg!(feature = "graphics") as u8;
    if n != 1 {
        "GW2 Enhanced"
    } else if cfg!(feature = "translation") {
        tr("Translation", "Tradução")
    } else if cfg!(feature = "textures") {
        tr("Textures", "Texturas")
    } else {
        tr("Graphics", "Gráficos")
    }
}

/// The features built into this DLL, with the name shown next to their Logs switch.
fn log_features() -> Vec<(Feature, &'static str)> {
    let mut v = vec![(Feature::Host, tr("GW2 Enhanced (overlay, updates)", "GW2 Enhanced (overlay, atualizações)"))];
    #[cfg(feature = "translation")]
    v.push((Feature::Translation, tr("Translation", "Tradução")));
    #[cfg(feature = "textures")]
    v.push((Feature::Textures, tr("Textures", "Texturas")));
    #[cfg(feature = "graphics")]
    v.push((Feature::Graphics, tr("Graphics", "Gráficos")));
    v
}

/// One Logs switch per feature (off by default, saved, live), and the recent problems kept while the files are off.
fn log_switches(ui: &Ui) {
    ui.text("Logs");
    {
        let c = ui.style_color(hudhook::imgui::StyleColor::TextDisabled);
        let _c = ui.push_style_color(hudhook::imgui::StyleColor::Text, c);
        ui.text_wrapped(tr(
            "Off by default. Turn a log on before reporting a problem; it is written to the GW2Enhanced and feature folders next to the game.",
            "Desligados por padrão. Ligue o log antes de relatar um problema; ele é gravado nas pastas GW2Enhanced e das funções, ao lado do jogo.",
        ));
    }
    for (f, name) in log_features() {
        let mut on = f.log().enabled();
        if ui.checkbox(format!("{name}##log-{}", f.key()), &mut on) {
            if let Err(e) = logs::choose(f, on) {
                registry::report("overlay", "logs", State::Degraded(format!("could not save {}: {e}", f.key())));
            }
        }
        if on && let Some(p) = f.log().path() {
            ui.same_line();
            ui.text_disabled(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        }
    }
    let recent: Vec<(&'static str, Vec<String>)> =
        log_features().into_iter().map(|(f, name)| (name, f.log().recent())).filter(|(_, r)| !r.is_empty()).collect();
    if !recent.is_empty()
        && ui.collapsing_header(
            tr("Recent problems###recent-problems", "Problemas recentes###recent-problems"),
            hudhook::imgui::TreeNodeFlags::empty(),
        )
    {
        for (name, lines) in recent {
            ui.text(name);
            for l in lines.iter().rev().take(10) {
                ui.text_wrapped(l);
            }
        }
    }
}

/// Waits for the features to publish their panels (they do it at different moments), then hooks `Present` once.
fn install_overlay(module: usize) {
    let start = Instant::now();
    let mut o = Overlay {
        #[cfg(feature = "translation")]
        translated: None,
        #[cfg(feature = "textures")]
        textures: None,
        #[cfg(feature = "graphics")]
        graphics: None,
    };
    while start.elapsed() < Duration::from_secs(40) {
        let mut all = true;
        #[cfg(feature = "translation")]
        {
            o.translated = o.translated.take().or_else(|| {
                gw2translated::overlay::PANEL
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
            });
            all &= o.translated.is_some();
        }
        #[cfg(feature = "textures")]
        {
            o.textures = o.textures.take().or_else(|| {
                gw2remastered::overlay::PANEL
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
            });
            all &= o.textures.is_some();
        }
        #[cfg(feature = "graphics")]
        {
            o.graphics = o.graphics.take().or_else(|| {
                gw2graphics::overlay::PANEL
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
            });
            all &= o.graphics.is_some();
        }
        if all && start.elapsed() > Duration::from_secs(8) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut any = false;
    #[cfg(feature = "translation")]
    {
        any |= o.translated.is_some();
    }
    #[cfg(feature = "textures")]
    {
        any |= o.textures.is_some();
    }
    #[cfg(feature = "graphics")]
    {
        any |= o.graphics.is_some();
    }
    if !any {
        registry::report("overlay", "", State::Off("no feature published a panel".into()));
        return;
    }
    let hooks = Hudhook::builder()
        .with::<ImguiDx11Hooks>(o)
        .with_hmodule(HINSTANCE(module as *mut _))
        .build();
    // Graphics must not adopt hudhook's dummy device and swapchain as the game's.
    #[cfg(feature = "graphics")]
    let r = gw2graphics::overlay::while_hooking_present(|| hooks.apply());
    #[cfg(not(feature = "graphics"))]
    let r = hooks.apply();
    if let Err(e) = r {
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
