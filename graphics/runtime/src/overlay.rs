//! In-game overlay (Dear ImGui via `hudhook`, same stack as GW2 Translated and Remastered).
//!
//! `hudhook` hooks `Present` and runs `render` once per frame just before the overlay is drawn,
//! so `render` also calls `recorder::frame_end` (the frame tick) whether or not the window is open.
//! Everything here runs on the render thread: no blocking, no panics (each part is wrapped in
//! `catch_unwind`), and the window is only built while it is open.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use hudhook::imgui::{Condition, Context, Io, StyleColor, TreeNodeFlags, Ui};
use hudhook::{ImguiRenderLoop, MessageFilter, RenderContext};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::{Config, HdrOutput};
use crate::output::OutputMode;
use crate::{ao, calib, d3d, datapath, depthstats, devshot, furmask, furpick, gate, halo, hdr10, hdr_shaders, log, output, photo, recorder, resolve, shaders, shafts, ssr, upgrade};

pub const PAPER_WHITE_RANGE: (f32, f32) = (80.0, 400.0);
pub const PEAK_RANGE: (f32, f32) = (400.0, 4000.0);
pub const BLOOM_RANGE: (f32, f32) = (0.0, 2.0);

/// Clamps the three sliders to the caps (the visibility rule: nothing may exceed them).
pub fn clamp_levels(paper_white: f32, peak: f32, bloom: f32) -> (f32, f32, f32) {
    let f = |v: f32, r: (f32, f32), d: f32| if v.is_finite() { v.clamp(r.0, r.1) } else { d };
    (f(paper_white, PAPER_WHITE_RANGE, 203.0), f(peak, PEAK_RANGE, 1000.0), f(bloom, BLOOM_RANGE, 1.0))
}

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 2;
const MOD_ALT: u32 = 4;

/// One key: a name (`End`, `Insert`, `F1`..`F24`, ...), a letter or digit, or a virtual code (`0x24`, `36`).
fn parse_vk(v: &str) -> Option<u32> {
    let l = v.trim().to_ascii_lowercase();
    let named = match l.as_str() {
        "home" => Some(0x24),
        "end" => Some(0x23),
        "insert" | "ins" => Some(0x2D),
        "delete" | "del" => Some(0x2E),
        "pageup" | "pgup" => Some(0x21),
        "pagedown" | "pgdn" => Some(0x22),
        "pause" => Some(0x13),
        "scrolllock" => Some(0x91),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if l.len() == 1 && l.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(l.as_bytes()[0].to_ascii_uppercase() as u32);
    }
    if let Some(n) = l.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
        return Some(0x70 + n - 1);
    }
    match l.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => l.parse().ok(),
    }
}

/// `Shift+End`, `Ctrl+Alt+F5`, `0x24`... -> (virtual key, modifier bits).
pub fn parse_key(v: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut key = None;
    for part in v.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "shift" => mods |= MOD_SHIFT,
            "ctrl" | "control" => mods |= MOD_CTRL,
            "alt" => mods |= MOD_ALT,
            p => key = Some(parse_vk(p)?),
        }
    }
    key.map(|k| (k, mods))
}

static VISIBLE: AtomicBool = AtomicBool::new(false);

/// The overlay window is open.
pub fn visible() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}
/// Opens or closes the window (the host uses it when graphics is the only feature built in).
pub fn set_visible(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
}
static KEY: AtomicU32 = AtomicU32::new(0x23);
static MODS: AtomicU32 = AtomicU32::new(MOD_SHIFT);
/// Photo mode hotkey (`photo_key`, default Ctrl+Shift+End).
static PHOTO_KEY: AtomicU32 = AtomicU32::new(0x23);
static PHOTO_MODS: AtomicU32 = AtomicU32::new(MOD_CTRL | MOD_SHIFT);

/// An overlay colour in the output's encoding: under HDR10 the overlay is drawn after the final pass, so ImGui colours would be PQ code
/// values (white = 10000 nits); they are mapped to paper white. scRGB and SDR are untouched.
fn ui_col(c: [f32; 4]) -> [f32; 4] {
    if output::hdr10_active() { hdr10::ui_color(c, output::paper_white()) } else { c }
}

fn game_has_focus() -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == unsafe { GetCurrentProcessId() }
}

fn yes(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// The graphics panel and per-frame work, drawn by a render loop (here or in the unified overlay).
pub struct Overlay {
    key_down: bool,
    cfg: Config,
    path: PathBuf,
    /// Result of the last Save click.
    note: String,
    /// The "Pick fur shaders" window is wanted (it is drawn while the main window is open).
    picker_open: bool,
    /// The picker window was actually drawn this frame (it keeps the draw hooks collecting, see `furpick`).
    picker_shown: bool,
    /// The row the player clicked in the picker (stays highlighted until clicked again).
    picker_sel: Option<furpick::HighlightKey>,
    /// Edit buffers of the fur text inputs; re-synced from the config while the input is not being edited.
    vs_buf: String,
    uv_buf: String,
    add_buf: String,
    /// Edit buffer of the rim shader list's add input.
    rim_add_buf: String,
    vs_active: bool,
    uv_active: bool,
    /// The photo hotkey was down at the last look (edge detection).
    photo_key_down: bool,
    /// The patch the player says is the dimmest one visible in the black-level pattern (guidance text).
    black_pick: i32,
    /// Style colours as ImGui made them, and the paper white / HDR10 state they were last mapped for.
    base_colors: Option<[[f32; 4]; 53]>,
    mapped_for: Option<(bool, u32)>,
}

fn key_paper(key: (bool, u32)) -> f32 {
    f32::from_bits(key.1)
}

/// Small dimmed paragraph, wrapped at the window edge (never clipped).
fn help(ui: &Ui, text: &str) {
    let c = ui.style_color(StyleColor::TextDisabled);
    let _t = ui.push_style_color(StyleColor::Text, c);
    ui.text_wrapped(text);
}

/// A collapsing section header; the body is only built while it is open.
fn section(ui: &Ui, label: &str, open_by_default: bool) -> bool {
    ui.collapsing_header(label, if open_by_default { TreeNodeFlags::DEFAULT_OPEN } else { TreeNodeFlags::empty() })
}

impl Overlay {
    pub fn new(cfg: Config, path: PathBuf) -> Overlay {
        Overlay {
            key_down: false,
            cfg,
            path,
            note: String::new(),
            picker_open: false,
            picker_shown: false,
            picker_sel: None,
            vs_buf: String::new(),
            uv_buf: String::new(),
            add_buf: String::new(),
            rim_add_buf: String::new(),
            vs_active: false,
            uv_active: false,
            photo_key_down: false,
            black_pick: -1,
            base_colors: None,
            mapped_for: None,
        }
    }

    /// Photo hotkey: edge triggered, exact modifiers, only with the game focused; works with the overlay closed.
    fn poll_photo_key(&mut self) {
        let held = |vk: i32| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0;
        let down = photo::combo_down(held, PHOTO_KEY.load(Ordering::Relaxed), PHOTO_MODS.load(Ordering::Relaxed));
        if down && !self.photo_key_down && game_has_focus() {
            let s = photo::request(photo::PhotoKind::Single);
            log!("overlay: photo hotkey: {s}");
        }
        self.photo_key_down = down;
    }

    /// Under HDR10 the style colours are mapped to PQ (see `ui_col`); back to the originals otherwise.
    fn map_style(&mut self, ctx: &mut Context) {
        let hdr10 = output::hdr10_active();
        let key = (hdr10, output::paper_white().to_bits());
        if self.mapped_for == Some(key) {
            return;
        }
        let base = *self.base_colors.get_or_insert_with(|| ctx.style().colors);
        let st = ctx.style_mut();
        for (i, c) in base.iter().enumerate() {
            st.colors[i] = if hdr10 { hdr10::ui_color(*c, key_paper(key)) } else { *c };
        }
        self.mapped_for = Some(key);
    }

    pub fn poll_key(&mut self) {
        let held = |vk: i32| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0;
        // Modifiers must match exactly, so the bare key (a game binding) never opens the overlay.
        let down = photo::combo_down(held, KEY.load(Ordering::Relaxed), MODS.load(Ordering::Relaxed));
        if down && !self.key_down && game_has_focus() {
            VISIBLE.fetch_xor(true, Ordering::Relaxed);
        }
        self.key_down = down;
    }

    /// v17: the on / off checkbox at the top of an effect section: the slider values are kept, the effect reads as strength 0 (or
    /// neutral) while off (toggles.rs), live, saved with Save (`<effect>_enabled`).
    fn fx_toggle(&mut self, ui: &Ui, fx: crate::toggles::Fx, label: &str) {
        let mut on = self.cfg.toggles.get(fx);
        if ui.checkbox(format!("{label} on##fx{}", fx.index()), &mut on) {
            self.cfg.toggles.0[fx.index()] = on;
            if crate::toggles::apply(&self.cfg.toggles) {
                hdr_shaders::set_bloom_strength(self.cfg.bloom_strength);
            }
        }
        if crate::toggles::master_off() {
            help(ui, "(all effects are off by the master switch in General)");
        }
    }

    /// The rim selector as the config says it, applied live (the same call hot reload makes).
    fn apply_rim(&self) {
        furmask::configure_rim(&self.cfg.rim_ps, self.cfg.rim_min_vcb, self.cfg.rim_debug);
    }

    /// The fur selector as the config says it, applied live (the same call hot reload makes).
    fn apply_fur(&self) {
        furmask::configure(&self.cfg.fur_ps, &self.cfg.fur_vs, self.cfg.fur_min_vcb, self.cfg.fur_debug);
    }

    /// The panel's content, without a window around it (the unified overlay puts it in a tab).
    pub fn body(&mut self, ui: &Ui) {
        let scale = furpick::ui_scale(ui.io().display_size[1]);
        // Negative item width: the widget takes the window width minus the label room, so labels are never cut.
        let _iw = ui.push_item_width(furpick::item_width(ui.window_size()[0], scale));
        if section(ui, "General / HDR", true) {
            // One switch for the whole graphics mod (saved as `enabled`): off stops the effects AND the float targets, HDR
            // shaders and resolve, so it is also the A/B against the engine. The per-effect switches below are kept.
            let mut on = self.cfg.enabled && !crate::toggles::master_off();
            if ui.checkbox("Graphics on", &mut on) {
                self.set_graphics_on(on);
            }
            help(ui, "Off: the game's own look (next frame). With HDR output the final conversion stays on (plain SDR brightness); a full revert of HDR output needs a restart.");
            help(ui, &format!("Effects: {}", crate::toggles::status_text()));
            self.sec_general(ui);
        }
        if section(ui, "Bloom + highlight", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Bloom, "Bloom");
            self.fx_toggle(ui, crate::toggles::Fx::Highlight, "Highlight boost");
            self.sec_bloom(ui);
        }
        if section(ui, "Ambient occlusion (AO)", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Ao, "AO");
            self.sec_ao(ui);
        }
        if section(ui, "Halo", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Halo, "Halo");
            self.sec_halo(ui);
        }
        if section(ui, "Reflections (SSR)", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Ssr, "Reflections");
            self.sec_ssr(ui);
        }
        if section(ui, "Sun shafts", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Shafts, "Sun shafts");
            self.sec_shafts(ui);
        }
        if section(ui, "Contact shadows", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Contact, "Contact shadows");
            self.sec_contact(ui);
        }
        if section(ui, "Aerial perspective", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Aerial, "Aerial perspective");
            self.sec_aerial(ui);
        }
        if section(ui, "Exposure & tone", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Exposure, "Auto exposure");
            self.fx_toggle(ui, crate::toggles::Fx::LocalTone, "Local tone mapping");
            self.sec_exposure(ui);
        }
        if section(ui, "Lens effects", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Lens, "Lens effects");
            self.sec_lens(ui);
        }
        if section(ui, "Color grading", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Grading, "Colour grading / LUT");
            self.sec_grade(ui);
        }
        if section(ui, "Anti-aliasing & filtering", false) {
            self.sec_aa(ui);
        }
        if section(ui, "Fur", false) {
            self.fx_toggle(ui, crate::toggles::Fx::FurLook, "Fur look");
            self.fx_toggle(ui, crate::toggles::Fx::Shells, "Shell fur");
            self.sec_fur(ui);
        }
        if section(ui, "Rim light & skin", false) {
            self.fx_toggle(ui, crate::toggles::Fx::Rim, "Rim light");
            self.sec_rim(ui);
        }
        if section(ui, "Photo mode", false) {
            self.sec_photo(ui);
        }
        if section(ui, "HDR calibration", false) {
            self.sec_calib(ui);
        }
        if section(ui, "Debug", false) {
            self.sec_debug(ui);
        }
        ui.separator();
        self.footer(ui);
    }

    fn window(&mut self, ui: &Ui) {
        let [dw, dh] = ui.io().display_size;
        let width = furpick::window_width(dw, dh);
        let mut open = true;
        ui.window("GW2 Enhanced Graphics")
            .size([width, 0.0], Condition::FirstUseEver)
            .size_constraints([300.0, 160.0], [dw.max(300.0), dh.max(160.0)])
            .position([60.0, 60.0], Condition::FirstUseEver)
            .opened(&mut open)
            .build(|| self.body(ui));
        if !open {
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }

    /// The single on / off switch of the graphics mod.
    fn set_graphics_on(&mut self, on: bool) {
        self.cfg.enabled = on;
        gate::set_user_enabled(on);
        if crate::toggles::set_master_off(!on) {
            hdr_shaders::set_bloom_strength(self.cfg.bloom_strength);
        }
        // Off: with an scRGB swapchain (cannot be reverted live) the final pass shows plain SDR at the
        // Windows SDR brightness; On: back to the configured paper white and peak.
        output::apply_user_levels(on, self.cfg.paper_white_nits, self.cfg.peak_nits);
        log!("overlay: graphics {}", if on { "on" } else { "off" });
    }

    fn sec_general(&mut self, ui: &Ui) {
        // v18: the diagnostic A/B of the float render-target upgrade (applies at the next game start; Save writes it)
        let mut up = self.cfg.hdr_upgrade;
        if ui.checkbox("Float scene upgrade (restart the game; off = engine-identical, nothing runs)", &mut up) {
            self.cfg.hdr_upgrade = up;
        }
        if crate::upgrade::forced_off() {
            help(ui, "Running with hdr_upgrade=0: the engine's own formats, no substitute, no effect (diagnostic).");
        }
        // v18: the clamp emulation (live)
        let mut cl = self.cfg.hdr_clamp;
        if ui.checkbox("8-bit clamp for engine shaders (fixes black volumes; off = A/B)", &mut cl) {
            self.cfg.hdr_clamp = cl;
            crate::clampemu::set_enabled(cl);
        }
        help(ui, &format!("Clamp emulation: {}", crate::clampemu::status_text()));
        let mut idx = match self.cfg.hdr_output {
            HdrOutput::Auto => 0,
            HdrOutput::On => 1,
            HdrOutput::Off => 2,
        };
        if ui.combo_simple_string("HDR output", &mut idx, &["Auto", "On", "Off"]) {
            self.cfg.hdr_output = [HdrOutput::Auto, HdrOutput::On, HdrOutput::Off][idx.min(2)];
        }
        help(ui, "Click Save, then restart the game: the swapchain is created once at start.");
        let (mut pw, mut peak) = (self.cfg.paper_white_nits, self.cfg.peak_nits);
        // The values follow the slider while dragging, but are applied once, when the edit is
        // committed (mouse released / Enter): applying re-compiles a shader on the render thread.
        let c1 = ui.slider("Paper white (nits)", PAPER_WHITE_RANGE.0, PAPER_WHITE_RANGE.1, &mut pw);
        let k1 = ui.is_item_deactivated_after_edit();
        let c2 = ui.slider("Peak (nits)", PEAK_RANGE.0, PEAK_RANGE.1, &mut peak);
        let k2 = ui.is_item_deactivated_after_edit();
        if c1 || c2 {
            let (pw, peak, _) = clamp_levels(pw, peak, self.cfg.bloom_strength);
            self.cfg.paper_white_nits = pw;
            self.cfg.peak_nits = peak;
        }
        if k1 || k2 {
            // Headroom follows at the next frame boundary; a paper white change also
            // re-installs the final-pass substitute (the old one keeps running until the new one exists).
            if output::mode() == OutputMode::ScRgb && self.cfg.enabled {
                output::set_levels(self.cfg.paper_white_nits, self.cfg.peak_nits);
            }
        }
        help(ui, "Paper white and peak act in HDR output only. Values apply when you release the slider.");
        // Black level: the SDR decode gamma of the final pass (0 = the piecewise sRGB curve Windows uses).
        let mut sg = self.cfg.sdr_gamma;
        let cg = ui.slider("SDR gamma (black level, 0 = sRGB)", 0.0, output::SDR_GAMMA_RANGE.1, &mut sg);
        let kg = ui.is_item_deactivated_after_edit();
        if cg {
            self.cfg.sdr_gamma = output::clamp_sdr_gamma(sg);
        }
        if kg {
            self.cfg.sdr_gamma = output::set_sdr_gamma(self.cfg.sdr_gamma);
        }
        help(ui, "2.2 = like an SDR monitor (deep shadows, default); 2.4 = deeper; 0 = the piecewise sRGB curve Windows uses for SDR content (lifted, greyer blacks). HDR output only.");
    }

    fn sec_bloom(&mut self, ui: &Ui) {
        let mut bloom = self.cfg.bloom_strength;
        let c3 = ui.slider("Bloom", BLOOM_RANGE.0, BLOOM_RANGE.1, &mut bloom);
        let k3 = ui.is_item_deactivated_after_edit();
        if c3 {
            let (_, _, bloom) = clamp_levels(self.cfg.paper_white_nits, self.cfg.peak_nits, bloom);
            self.cfg.bloom_strength = bloom;
        }
        if k3 {
            hdr_shaders::set_bloom_strength(self.cfg.bloom_strength);
        }
        help(ui, "Bloom applies when you release the slider.");
        let mut hb = self.cfg.highlight_boost;
        if ui.slider("Highlight boost", 1.0, 4.0, &mut hb) {
            self.cfg.highlight_boost = hb.clamp(1.0, 4.0);
            resolve::set_highlight(self.cfg.highlight_boost);
        }
        help(ui, "Bright pixels (largest channel from 0.4 of SDR white, full from 0.9) are multiplied up to the display headroom at 4; lights get the whole gain, big bright surfaces 30 % of it. 1 = no boost. HDR only. Applies live.");
        ui.text_wrapped(format!("Highlight: {}", crate::datapath::highlight_status_text()));
    }

    fn sec_ao(&mut self, ui: &Ui) {
        let (mut aos, mut aor) = (self.cfg.ao_strength, self.cfg.ao_radius);
        // AO values go to atomics read by the render thread; the shaders are never recompiled.
        if ui.slider("AO strength", ao::STRENGTH_RANGE.0, ao::STRENGTH_RANGE.1, &mut aos) {
            self.cfg.ao_strength = ao::set_strength(aos);
        }
        if ui.slider("AO radius", ao::RADIUS_RANGE.0, ao::RADIUS_RANGE.1, &mut aor) {
            self.cfg.ao_radius = ao::set_radius(aor);
        }
        help(ui, "AO strength 0 turns the pass off. Applies live.");
    }

    fn sec_halo(&mut self, ui: &Ui) {
        let (mut hs, mut hr) = (self.cfg.halo_strength, self.cfg.halo_radius);
        // Halo values go to atomics read by the render thread; the shaders are never recompiled.
        if ui.slider("Halo strength", halo::STRENGTH_RANGE.0, halo::STRENGTH_RANGE.1, &mut hs) {
            self.cfg.halo_strength = halo::set_strength(hs);
        }
        if ui.slider("Halo radius", halo::RADIUS_RANGE.0, halo::RADIUS_RANGE.1, &mut hr) {
            self.cfg.halo_radius = halo::set_radius(hr);
        }
        help(ui, "Halo strength 0 turns the whole feature off (no passes). Applies live.");
    }

    fn sec_ssr(&mut self, ui: &Ui) {
        let (mut ss, mut sr) = (self.cfg.ssr_strength, self.cfg.ssr_range);
        if ui.slider("Reflection strength", ssr::STRENGTH_RANGE.0, ssr::STRENGTH_RANGE.1, &mut ss) {
            self.cfg.ssr_strength = ssr::set_strength(ss);
        }
        if ui.slider("Reflection range", ssr::RANGE_RANGE.0, ssr::RANGE_RANGE.1, &mut sr) {
            self.cfg.ssr_range = ssr::set_range(sr);
        }
        help(ui, "Reflection strength 0 turns the passes off. Applies live.");
        let (mut wb, mut wd) = (self.cfg.ssr_water_boost, self.cfg.water_depth_look);
        if ui.slider("Water / glass boost", ssr::WATER_RANGE.0, ssr::WATER_RANGE.1, &mut wb) {
            self.cfg.ssr_water_boost = ssr::set_water_boost(wb);
        }
        if ui.slider("Water depth look", ssr::DEPTH_LOOK_RANGE.0, ssr::DEPTH_LOOK_RANGE.1, &mut wd) {
            self.cfg.water_depth_look = ssr::set_depth_look(wd);
        }
        help(ui, "Only on surfaces the score calls water or glass: Fresnel curve (at most 0.85), water tint, sky instead of a black band at the screen edge, sharp mirror on calm water / soft on ripples. Boost 0 = the old reflections exactly. Depth look darkens such water by up to 15 % where the reflection is strong (needs the boost).");
        let mut sm = self.cfg.ssr_smart;
        if ui.slider("Material score amount", ssr::SMART_RANGE.0, ssr::SMART_RANGE.1, &mut sm) {
            self.cfg.ssr_smart = ssr::set_smart(sm);
        }
        let (mut sh, mut sf) = (self.cfg.ssr_hue_filter, self.cfg.ssr_flatness);
        if ui.slider("Hue filter (green, earth)", ssr::HUE_RANGE.0, ssr::HUE_RANGE.1, &mut sh) {
            self.cfg.ssr_hue_filter = ssr::set_hue_filter(sh);
        }
        if ui.slider("Flatness (varying normals)", ssr::FLAT_RANGE.0, ssr::FLAT_RANGE.1, &mut sf) {
            self.cfg.ssr_flatness = ssr::set_flatness(sf);
        }
        let mut mc = self.cfg.ssr_matte_cut;
        if ui.slider("Matte cut (dry ground)", ssr::MATTE_RANGE.0, ssr::MATTE_RANGE.1, &mut mc) {
            self.cfg.ssr_matte_cut = ssr::set_matte_cut(mc);
        }
        help(ui, "Matte cut: no gloss buffer exists, so moonlight, fog and shade make dry dirt look like water or polished stone. 0.8 (default) keeps reflections only where the surface still looks like water / glass / ice under the frame's own light; 1 = strictest; lower (0.3..0.5) brings grey polished floors back. 0 = the old score. Applies live.");
        let mut cls = self.cfg.ssr_classes;
        let mut changed = false;
        for (bit, label) in [(ssr::CLASS_LOW, "Reflect on: low (blue)"), (ssr::CLASS_MID, "Reflect on: mid (green)"), (ssr::CLASS_HIGH, "Reflect on: high (red)")] {
            let mut on = cls & bit != 0;
            if ui.checkbox(label, &mut on) {
                cls = if on { cls | bit } else { cls & !bit };
                changed = true;
            }
        }
        if changed {
            self.cfg.ssr_classes = ssr::set_classes(cls);
        }
        let mut drefl = resolve::debug_reflectivity();
        if ui.checkbox("Reflectivity map", &mut drefl) {
            resolve::set_debug_reflectivity(drefl);
        }
        ui.text_colored(ui_col([0.35, 0.7, 1.0, 1.0]), "blue / cyan = reflective");
        ui.text_colored(ui_col([0.6, 0.6, 0.6, 1.0]), "black = not reflective");
        help(ui, "The map shows what the detector picks (blue 0 .. red 1). 'Material classes' in Debug shows the alpha classes. Save to keep.");
    }

    fn sec_shafts(&mut self, ui: &Ui) {
        let (mut shs, mut shl) = (self.cfg.shafts_strength, self.cfg.shafts_length);
        if ui.slider("Sun shafts", shafts::STRENGTH_RANGE.0, shafts::STRENGTH_RANGE.1, &mut shs) {
            self.cfg.shafts_strength = shafts::set_strength(shs);
        }
        if ui.slider("Shaft length", shafts::LENGTH_RANGE.0, shafts::LENGTH_RANGE.1, &mut shl) {
            self.cfg.shafts_length = shafts::set_length(shl);
        }
        help(ui, "Sun shafts 0 turns the feature off. Applies live. Debug view marks the sun with a cross.");
    }

    fn sec_contact(&mut self, ui: &Ui) {
        use crate::contact as ct;
        let (mut s, mut l) = (self.cfg.contact_strength, self.cfg.contact_length);
        // Atomics read by the render thread; nothing is recompiled.
        if ui.slider("Contact shadow strength", ct::STRENGTH_RANGE.0, ct::STRENGTH_RANGE.1, &mut s) {
            self.cfg.contact_strength = ct::set_strength(s);
        }
        if ui.slider("Contact shadow length", ct::LENGTH_RANGE.0, ct::LENGTH_RANGE.1, &mut l) {
            self.cfg.contact_length = ct::set_length(l);
        }
        let mut fixed = self.cfg.contact_sun == "fixed";
        if ui.checkbox("Fixed light direction (ignore the sun estimate)", &mut fixed) {
            self.cfg.contact_sun = ct::set_mode(if fixed { "fixed" } else { "auto" }).to_string();
        }
        help(ui, "Contact shadows 0 turns the feature off. A short ray march in the depth buffer towards the light; never under water, in fog, on lights or the sky. Applies live; Save keeps it.");
        ui.text_wrapped(format!("Status: {}", ct::status_text()));
    }

    fn sec_aerial(&mut self, ui: &Ui) {
        use crate::aerial as ae;
        let (mut s, mut d) = (self.cfg.aerial_strength, self.cfg.aerial_distance);
        if ui.slider("Aerial perspective", ae::STRENGTH_RANGE.0, ae::STRENGTH_RANGE.1, &mut s) {
            self.cfg.aerial_strength = ae::set_strength(s);
        }
        if ui.slider("Half-strength distance (m)", ae::DISTANCE_RANGE.0, 2000.0, &mut d) {
            self.cfg.aerial_distance = ae::set_distance(d);
        }
        let mut u = self.cfg.depth_unit;
        if ui.slider("Depth unit (m at the far plane)", ae::UNIT_RANGE.0, 5000.0, &mut u) {
            self.cfg.depth_unit = ae::set_unit(u);
        }
        help(ui, "Aerial perspective 0 turns the feature off. A subtle haze toward the game's own fog colour on pixels farther than 15 m (at most 35 %); never on the sky, lights, in dense fog or under water. The engine's depth is normalised (1.0 = far plane = sky): the depth unit is how many metres 1.0 is (1000 = default, measured; also scales the contact shadow length). Applies live; Save keeps it.");
        ui.text_wrapped(format!("Status: {}", ae::status_text()));
    }

    fn sec_exposure(&mut self, ui: &Ui) {
        use crate::exposure as ex;
        let (mut s, mut t, mut r) = (self.cfg.autoexp_strength, self.cfg.autoexp_target, self.cfg.autoexp_range);
        // All values go to atomics read by the render thread; nothing is recompiled.
        if ui.slider("Auto exposure", ex::STRENGTH_RANGE.0, ex::STRENGTH_RANGE.1, &mut s) {
            self.cfg.autoexp_strength = ex::set_strength(s);
        }
        if ui.slider("Target mid-grey", ex::TARGET_RANGE.0, ex::TARGET_RANGE.1, &mut t) {
            self.cfg.autoexp_target = ex::set_target(t);
        }
        if ui.slider("Max change (EV)", ex::RANGE_RANGE.0, ex::RANGE_RANGE.1, &mut r) {
            self.cfg.autoexp_range = ex::set_range(r);
        }
        help(ui, "Auto exposure 0 = off. Eye adaptation of the 3D scene (not the UI) towards the target; the change is limited to the max EV, brightening to +1 EV. Applies live.");
        let (mut up, mut down) = (self.cfg.adapt_up, self.cfg.adapt_down);
        if ui.slider("Adapt to brighter (s)", ex::ADAPT_RANGE.0, 5.0, &mut up) {
            self.cfg.adapt_up = ex::set_adapt_up(up);
        }
        if ui.slider("Adapt to darker (s)", ex::ADAPT_RANGE.0, 5.0, &mut down) {
            self.cfg.adapt_down = ex::set_adapt_down(down);
        }
        help(ui, "Time constants: about 95 % of a change after three times the value. The eye is slower going into the dark.");
        let mut lt = self.cfg.localtone_strength;
        if ui.slider("Local tone mapping", ex::LOCAL_RANGE.0, ex::LOCAL_RANGE.1, &mut lt) {
            self.cfg.localtone_strength = ex::set_local_strength(lt);
        }
        help(ui, "0 = off. Compresses big bright areas (sand, snow, white fur) without flattening their detail, edge-aware (no halos). Lights keep their energy. Applies live.");
        ui.text_wrapped(format!("Auto exposure: {}", ex::status_text()));
        ui.text_wrapped(format!("Local tone: {}", ex::status_local_text()));
    }

    fn sec_lens(&mut self, ui: &Ui) {
        use crate::lens as ln;
        let (mut s, mut t, mut g, mut l) = (self.cfg.lens_strength, self.cfg.lens_threshold, self.cfg.lens_ghost, self.cfg.lens_streak_length);
        // Atomics read by the render thread; nothing is recompiled.
        if ui.slider("Streak / glare strength", ln::STRENGTH_RANGE.0, ln::STRENGTH_RANGE.1, &mut s) {
            self.cfg.lens_strength = ln::set_strength(s);
        }
        if ui.slider("Threshold (x paper white)", ln::THRESHOLD_RANGE.0, ln::THRESHOLD_RANGE.1, &mut t) {
            self.cfg.lens_threshold = ln::set_threshold(t);
        }
        if ui.slider("Ghost flares", ln::GHOST_RANGE.0, ln::GHOST_RANGE.1, &mut g) {
            self.cfg.lens_ghost = ln::set_ghost(g);
        }
        if ui.slider("Streak length", ln::LENGTH_RANGE.0, ln::LENGTH_RANGE.1, &mut l) {
            self.cfg.lens_streak_length = ln::set_streak_length(l);
        }
        let (mut z, mut c) = (self.cfg.lens_max_size, self.cfg.lens_color_reject);
        if ui.slider("Largest source size (share of height)", ln::MAX_SIZE_RANGE.0, ln::MAX_SIZE_RANGE.1, &mut z) {
            self.cfg.lens_max_size = ln::set_max_size(z);
        }
        if ui.slider("Reject coloured glows", ln::COLOR_REJECT_RANGE.0, ln::COLOR_REJECT_RANGE.1, &mut c) {
            self.cfg.lens_color_reject = ln::set_color_reject(c);
        }
        help(ui, "Only compact light sources (fire, lamps, candles, the sun) get a flare: a bright area bigger than the largest source size (a glowing creature, a bright wall) gets none, 0 = no size test. Saturated blue, cyan, violet or green glows must be brighter (up to 1 + 4 x the reject value times the threshold); white and warm lights are not affected.");
        help(ui, "Strength 0 turns the whole feature off (no passes). Horizontal streaks and a soft glare from HDR highlights above the threshold (measured before the highlight boost, so white sand and snow never trigger); ghost flares only for very bright point sources (2x the threshold), 0 = no ghosts. Warm white, never blue; adds at most 0.6 of paper white. HDR output only. Applies live; Save keeps it.");
        ui.text_wrapped(format!("Status: {}", crate::look::status_lens()));
    }

    fn apply_grade(&mut self) {
        let g = crate::grading::set(self.cfg.grade());
        self.cfg.set_grade(&g);
    }

    fn sec_grade(&mut self, ui: &Ui) {
        use crate::grading as gr;
        ui.text("Presets");
        for (i, p) in gr::PRESETS.iter().enumerate() {
            if i > 0 {
                ui.same_line();
            }
            if ui.button(p.name) {
                self.cfg.set_grade(&p.grade);
                self.cfg.grade_preset = p.name.to_string();
                self.apply_grade();
            }
            if ui.is_item_hovered() {
                ui.tooltip_text(p.what);
            }
        }
        if !self.cfg.grade_preset.is_empty() {
            help(ui, &format!("Last preset: {} (the sliders below may have been moved since). Save writes grade_preset and every slider.", self.cfg.grade_preset));
        }
        let mut g = self.cfg.grade();
        let mut changed = false;
        changed |= ui.slider("Saturation", gr::SATURATION_RANGE.0, gr::SATURATION_RANGE.1, &mut g.saturation);
        changed |= ui.slider("Contrast", gr::CONTRAST_RANGE.0, gr::CONTRAST_RANGE.1, &mut g.contrast);
        changed |= ui.slider("Temperature (warm +)", gr::TEMPERATURE_RANGE.0, gr::TEMPERATURE_RANGE.1, &mut g.temperature);
        changed |= ui.slider("Tint (magenta +)", gr::TINT_RANGE.0, gr::TINT_RANGE.1, &mut g.tint);
        changed |= ui.slider("Vibrance", gr::VIBRANCE_RANGE.0, gr::VIBRANCE_RANGE.1, &mut g.vibrance);
        changed |= ui.slider("Shadows", gr::SHADOWS_RANGE.0, gr::SHADOWS_RANGE.1, &mut g.shadows);
        changed |= ui.slider("Highlights", gr::HIGHLIGHTS_RANGE.0, gr::HIGHLIGHTS_RANGE.1, &mut g.highlights);
        if changed {
            self.cfg.set_grade(&g);
            self.apply_grade();
        }
        help(ui, "All neutral = off (nothing runs). Applied to the 3D scene before the UI in linear light; contrast pivots at mid-grey 0.18 and stays linear above 1.0, so HDR highlights keep their hue and are not clipped. Applies live; Save keeps it.");
        let mut ls = self.cfg.grade_lut_strength;
        if ui.slider("LUT strength", gr::LUT_STRENGTH_RANGE.0, gr::LUT_STRENGTH_RANGE.1, &mut ls) {
            self.cfg.grade_lut_strength = gr::set_lut_strength(ls);
        }
        if matches!(gr::lut_state(), gr::LutState::Off) {
            ui.text_colored(ui_col([1.0, 0.7, 0.3, 1.0]), "No LUT loaded: the LUT strength does nothing. Pick one below.");
        }
        self.lut_picker(ui);
        ui.text_wrapped(format!("LUT file (grade_lut in config.ini): {}", if self.cfg.grade_lut.is_empty() { "none" } else { self.cfg.grade_lut.as_str() }));
        ui.text_wrapped(format!("LUT status: {}", gr::lut_status_text()));
        ui.text_wrapped(format!("Status: {}", crate::look::status_grade()));
    }

    /// LUT selector: the `.cube` files of GW2Graphics\luts (listed when the section opens and on Refresh, not every frame)
    /// and a button that writes the four example LUTs there (never overwriting). Picking one loads it live (background thread).
    fn lut_picker(&mut self, ui: &Ui) {
        use crate::grading as gr;
        use std::sync::Mutex;
        static LIST: Mutex<Option<Vec<String>>> = Mutex::new(None);
        static NOTE: Mutex<String> = Mutex::new(String::new());
        let Some(dir) = gr::dir() else {
            ui.text_wrapped("LUT folder: not known yet");
            return;
        };
        let mut list = LIST.lock().unwrap_or_else(|e| e.into_inner());
        if list.is_none() {
            *list = Some(gr::list_luts_in(&dir));
        }
        let files = list.clone().unwrap_or_default();
        let mut items: Vec<String> = vec!["(none)".to_string()];
        items.extend(files.iter().cloned());
        let cur = self.cfg.grade_lut.trim().replace('\\', "/");
        let mut idx = files.iter().position(|f| f.eq_ignore_ascii_case(&cur)).map_or(0, |i| i + 1);
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        if ui.combo_simple_string("LUT (GW2Graphics\\luts)", &mut idx, &refs) {
            self.cfg.grade_lut = if idx == 0 { String::new() } else { files[idx - 1].clone() };
            gr::set_lut(&self.cfg.grade_lut);
        }
        if ui.button("Refresh list") {
            *list = Some(gr::list_luts_in(&dir));
        }
        ui.same_line();
        if ui.button("Create example LUTs") {
            let msg = match gr::write_example_luts_in(&dir) {
                Ok(0) => "The example LUTs are already there (existing files are never overwritten).".to_string(),
                Ok(n) => format!("{n} example LUTs written to GW2Graphics\\luts: identity, warm, cool, teal-orange."),
                Err(e) => format!("Could not write the example LUTs: {e}"),
            };
            *NOTE.lock().unwrap_or_else(|e| e.into_inner()) = msg;
            *list = Some(gr::list_luts_in(&dir));
        }
        if ui.is_item_hovered() {
            ui.tooltip_text("Writes identity / warm / cool / teal-orange .cube files (log2 shaper LUTs that keep the luminance) into GW2Graphics\\luts. Then pick one above and set the LUT strength.");
        }
        let note = NOTE.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !note.is_empty() {
            ui.text_wrapped(note);
        }
        if files.is_empty() {
            help(ui, "No .cube file in GW2Graphics\\luts yet: press Create example LUTs, or copy your own (log2 shaper, see docs/grading.md) there and press Refresh.");
        }
    }

    fn sec_aa(&mut self, ui: &Ui) {
        help(ui, "These keys are read from config.ini (and hot reloaded with the dev flow): smaa, smaa_threshold, sharpen_strength, aniso_level, mip_bias. Edit the file and save, or use the dev hot reload.");
        ui.text_wrapped(format!("Anti-aliasing: {}", crate::aa::status_text()));
        ui.text_wrapped(format!("Texture filtering: {}", crate::aniso::status_text()));
        ui.text_wrapped(format!(
            "smaa={} threshold={:.3} sharpen={:.2} aniso={} mip_bias={:.2}",
            self.cfg.smaa, self.cfg.smaa_threshold, self.cfg.sharpen_strength, self.cfg.aniso_level, self.cfg.mip_bias
        ));
    }

    fn sec_fur(&mut self, ui: &Ui) {
        help(ui, "Fur applies only to the draws chosen below. Use 'Pick fur shaders' to choose them from the live list of skinned draws. Everything applies live; Save keeps it.");
        let mut fs = self.cfg.fur_strength;
        if ui.slider("Fur strength", datapath::FUR_RANGE.0, datapath::FUR_RANGE.1, &mut fs) {
            self.cfg.fur_strength = datapath::set_fur(fs);
        }
        self.fur_look_controls(ui);
        self.shell_controls(ui);
        let mut dbg = self.cfg.fur_debug;
        if ui.checkbox("Show selection in magenta", &mut dbg) {
            self.cfg.fur_debug = dbg;
            self.apply_fur();
        }
        let mut v = self.cfg.fur_min_vcb.min(furmask::MAX_VCB) as i32;
        if ui.input_int("Min vertex cb (bytes)", &mut v).build() {
            self.cfg.fur_min_vcb = v.clamp(0, furmask::MAX_VCB as i32) as u32;
            self.apply_fur();
        }
        if !self.vs_active {
            self.vs_buf = self.cfg.fur_vs.clone();
        }
        if ui.input_text("Vertex shaders (optional)", &mut self.vs_buf).build() {
            self.cfg.fur_vs = self.vs_buf.clone();
            self.apply_fur();
        }
        self.vs_active = ui.is_item_active();
        if !self.uv_active {
            self.uv_buf = self.cfg.fur_uv.clone();
        }
        if ui.input_text("Mesh uv (auto / TEXCOORD0)", &mut self.uv_buf).build() {
            let t = self.uv_buf.trim().to_ascii_lowercase();
            self.cfg.fur_uv = if t.is_empty() { "auto".to_string() } else { t };
            furmask::set_uv_pref(&self.cfg.fur_uv);
        }
        self.uv_active = ui.is_item_active();

        ui.separator();
        ui.text("Fur pixel shaders (fur_ps)");
        let list = furmask::parse_hashes(&self.cfg.fur_ps);
        if list.is_empty() {
            help(ui, "Empty: nothing is fur.");
        }
        let mut remove = None;
        for h in &list {
            ui.text(format!("{h:08x}"));
            ui.same_line();
            if ui.small_button(format!("Remove##rm{h:08x}")) {
                remove = Some(*h);
            }
        }
        if let Some(h) = remove {
            self.cfg.fur_ps = furpick::list_remove(&self.cfg.fur_ps, h);
            self.apply_fur();
        }
        ui.input_text("##addhash", &mut self.add_buf).hint("hash, 1 to 8 hex digits").build();
        ui.same_line();
        if ui.button("Add") {
            if let Some(h) = furpick::parse_hash(&self.add_buf) {
                self.cfg.fur_ps = furpick::list_add(&self.cfg.fur_ps, h);
                self.apply_fur();
                self.add_buf.clear();
            }
        }
        if ui.button("Verified shaders") {
            self.cfg.fur_ps = furpick::CHARR_PRESET.to_string();
            self.apply_fur();
        }
        ui.same_line();
        if ui.button("Clear list") {
            self.cfg.fur_ps.clear();
            self.apply_fur();
        }
        ui.separator();
        if ui.checkbox("Pick fur shaders (opens a window)", &mut self.picker_open) && !self.picker_open {
            self.picker_sel = None;
        }
        ui.text_wrapped(format!("Fur mask: {}", furmask::status_text()));
    }

    /// Shell fur (docs/shell-fur.md): real strands out of the silhouette. Every control applies live (shellfur::configure); Save keeps them.
    fn shell_controls(&mut self, ui: &Ui) {
        use crate::shellfur as sf;
        ui.text("Shell fur (real strands; live; Save keeps them)");
        help(ui, "The fur draws picked below are drawn again as 4..32 shells extruded along the surface: strands that stick out of the silhouette, dark at the roots, back-lit against a bright sky (never in the dark). Needs fur strength > 0 and the fur_ps selection above. The line status_shells lists every fur draw and whether it got shells.");
        let mut c = self.cfg.shell.clone();
        let mut changed = false;
        let mut on = c.on != 0;
        if ui.checkbox("Shell fur on (fur_shell)", &mut on) {
            c.on = on as u8;
            changed = true;
        }
        let mut n = c.shells as i32;
        if ui.slider("Shells (fur_shells, 0 = off)", 0, sf::MAX_SHELLS as i32, &mut n) {
            c.shells = sf::clamp_shells(n as f32);
            changed = true;
        }
        changed |= ui.slider("Strand length cm (shell_length)", sf::LENGTH_RANGE.0, sf::LENGTH_RANGE.1, &mut c.length);
        changed |= ui.slider("Strand density (shell_density)", sf::DENSITY_RANGE.0, sf::DENSITY_RANGE.1, &mut c.density);
        changed |= ui.slider("Strand thickness (shell_thickness)", sf::THICKNESS_RANGE.0, sf::THICKNESS_RANGE.1, &mut c.thickness);
        changed |= ui.slider("Root darkening (shell_root_dark)", 0.0, 1.0, &mut c.root_dark);
        changed |= ui.slider("Wind (shell_wind)", 0.0, 1.0, &mut c.wind);
        changed |= ui.slider("Back light (shell_rim)", sf::RIM_RANGE.0, sf::RIM_RANGE.1, &mut c.rim);
        changed |= ui.slider("LOD distance m (shell_lod_distance)", sf::LOD_RANGE.0, sf::LOD_RANGE.1, &mut c.lod);
        changed |= ui.slider("Keep the face clear (shell_detail: eyes, nose, markings)", 0.0, 1.0, &mut c.detail);
        changed |= ui.slider("Clumping (shell_clump)", 0.0, 1.0, &mut c.clump);
        // v3: only the texels that really are fur (not the horns, claws, nose, lips); also the old fur look and the magenta fur debug.
        changed |= ui.input_text("Fur texel tests (shell_fur_mask: off / t0.a<, t0.f>0.025 ...)", &mut c.fur_mask).build();
        changed |= ui.slider("Fur threshold (shell_fur_threshold)", 0.0, 1.0, &mut c.fur_threshold);
        changed |= ui.slider("Fur threshold softness (shell_fur_soft)", sf::FUR_SOFT_RANGE.0, sf::FUR_SOFT_RANGE.1, &mut c.fur_soft);
        changed |= ui.input_text("No-fur uv rects (shell_uv_exclude: u0 v0 u1 v1; ...)", &mut c.uv_exclude).build();
        changed |= ui.slider("Engine units per metre (shell_unit, 0 = auto)", 0.0, 100.0, &mut c.unit);
        let mut dbg = c.debug == 1;
        if ui.checkbox("Show the shells (red root .. yellow tip)", &mut dbg) {
            c.debug = dbg as u8;
            changed = true;
        }
        let mut dbg2 = c.debug == 2;
        if ui.checkbox("Show the shells per draw (shell_debug=2: a colour per fur draw, black = not fur)", &mut dbg2) {
            c.debug = if dbg2 { 2 } else { 0 };
            changed = true;
        }
        changed |= ui.input_text("Shell pixel shaders (shell_ps, empty = fur_ps)", &mut c.ps).build();
        changed |= ui.input_text("Shell vertex shaders (shell_vs, empty = any)", &mut c.vs).build();
        let mut v = c.min_vcb.min(furmask::MAX_VCB) as i32;
        if ui.input_int("Shell min vertex cb (shell_min_vcb)", &mut v).build() {
            c.min_vcb = v.clamp(0, furmask::MAX_VCB as i32) as u32;
            changed = true;
        }
        changed |= ui.input_text("Index counts (shell_counts: 17694 takes, -5214 leaves out)", &mut c.counts).build();
        changed |= ui.input_text("Normal (shell_normal: face / TEXCOORDn)", &mut c.normal).build();
        if ui.button("Defaults##shell") {
            c = sf::ShellCfg::default();
            changed = true;
        }
        if changed {
            self.cfg.shell = c.clone();
            sf::configure(&c);
        }
        ui.text_wrapped(format!("Shells: {}", sf::status_text()));
        ui.separator();
    }

    /// The live fur look (docs/fur-look.md): six sliders (Save writes them) and the diagnostic view (never saved). All apply live.
    fn fur_look_controls(&mut self, ui: &Ui) {
        use datapath as dp;
        ui.separator();
        ui.text("Fur look (live; Save keeps the six sliders)");
        let (mut amp, mut scale, mut length, mut detail, mut rim, mut sss) = (self.cfg.fur_amp, self.cfg.fur_scale, self.cfg.fur_length, self.cfg.fur_detail, self.cfg.fur_rim, self.cfg.fur_sss);
        if ui.slider("Strand amplitude (fur_amp)", dp::FUR_AMP_RANGE.0, dp::FUR_AMP_RANGE.1, &mut amp) {
            self.cfg.fur_amp = dp::set_fur_amp(amp);
        }
        if ui.slider("Strand size (fur_scale)", dp::FUR_SCALE_RANGE.0, dp::FUR_SCALE_RANGE.1, &mut scale) {
            self.cfg.fur_scale = dp::set_fur_scale(scale);
        }
        if ui.slider("Strand length (fur_length)", dp::FUR_LENGTH_RANGE.0, dp::FUR_LENGTH_RANGE.1, &mut length) {
            self.cfg.fur_length = dp::set_fur_length(length);
        }
        if ui.slider("Texture detail (fur_detail)", dp::FUR_DETAIL_RANGE.0, dp::FUR_DETAIL_RANGE.1, &mut detail) {
            self.cfg.fur_detail = dp::set_fur_detail(detail);
        }
        if ui.slider("Outline glow (fur_rim)", dp::FUR_RIM_RANGE.0, dp::FUR_RIM_RANGE.1, &mut rim) {
            self.cfg.fur_rim = dp::set_fur_rim(rim);
        }
        if ui.slider("Warm skin glow (fur_sss)", dp::FUR_SSS_RANGE.0, dp::FUR_SSS_RANGE.1, &mut sss) {
            self.cfg.fur_sss = dp::set_fur_sss(sss);
        }
        help(ui, "Defaults 0.32 / 1 / 5 / 1 / 0.25 / 1. Outline glow 1 is the strong outline the fur had before, 0 removes the glow and the feather of the outline. Strand size multiplies the strand cells, length is relative to the width.");
        let mut view = dp::fur_view() as usize;
        if ui.combo_simple_string("Fur view (diagnostic)", &mut view, &["Off", "1 Mesh uv as colour (red = no uv)", "2 Strand pattern (grey)", "3 Rim band (white)", "4 Strand fade (white = full)"]) {
            self.cfg.fur_view = dp::set_fur_view(view.min(dp::FUR_VIEW_MAX as usize) as u8);
        }
        help(ui, "Painted on the pixels of the fur mask only, also with fur strength 0 (the selection must not be empty). Never saved. Switch 'Show selection in magenta' off to see it.");
        if ui.button("Defaults##furlook") {
            let d = dp::FurLook::default();
            self.cfg.fur_amp = d.amp;
            self.cfg.fur_scale = d.scale;
            self.cfg.fur_length = d.length;
            self.cfg.fur_detail = d.detail;
            self.cfg.fur_rim = d.rim;
            self.cfg.fur_sss = d.sss;
            dp::set_fur_look(&dp::FurLook { view: dp::fur_view(), ..d });
        }
        ui.text_wrapped(format!("Look: {}", dp::fur_look_text()));
        ui.separator();
    }

    fn sec_rim(&mut self, ui: &Ui) {
        help(ui, "A soft rim along the silhouette of characters (never above +25 % luminance, tinted a little towards the bright background) and a gentle warm glow of the dim-lit parts (at most +8 %). Never in the dark, in water or in fog; skipped for alpha-blended (stealth) draws. Strength 0 = off. Applies live; Save keeps it.");
        let (mut rs, mut rw, mut ss) = (self.cfg.rim_strength, self.cfg.rim_width, self.cfg.subsurface_strength);
        if ui.slider("Rim strength", datapath::RIM_RANGE.0, datapath::RIM_RANGE.1, &mut rs) {
            self.cfg.rim_strength = datapath::set_rim(rs);
        }
        if ui.slider("Rim width (px)", datapath::RIM_WIDTH_RANGE.0, datapath::RIM_WIDTH_RANGE.1, &mut rw) {
            self.cfg.rim_width = datapath::set_rim_width(rw);
        }
        if ui.slider("Subsurface glow", 0.0, 1.0, &mut ss) {
            self.cfg.subsurface_strength = datapath::set_subsurface(ss);
        }
        let mut dbg = self.cfg.rim_debug;
        if ui.checkbox("Show selection in cyan", &mut dbg) {
            self.cfg.rim_debug = dbg;
            self.apply_rim();
        }
        let mut v = self.cfg.rim_min_vcb.min(furmask::MAX_VCB) as i32;
        if ui.input_int("Rim min vertex cb (bytes)", &mut v).build() {
            self.cfg.rim_min_vcb = v.clamp(0, furmask::MAX_VCB as i32) as u32;
            self.apply_rim();
        }
        ui.separator();
        ui.text("Rim pixel shaders (rim_ps)");
        let list = furmask::parse_hashes(&self.cfg.rim_ps);
        if list.is_empty() {
            help(ui, "Empty: no rim light.");
        }
        let mut remove = None;
        for h in &list {
            ui.text(format!("{h:08x}"));
            ui.same_line();
            if ui.small_button(format!("Remove##rimrm{h:08x}")) {
                remove = Some(*h);
            }
        }
        if let Some(h) = remove {
            self.cfg.rim_ps = furpick::list_remove(&self.cfg.rim_ps, h);
            self.apply_rim();
        }
        ui.input_text("##addrimhash", &mut self.rim_add_buf).hint("hash, 1 to 8 hex digits").build();
        ui.same_line();
        if ui.button("Add##rimadd") {
            if let Some(h) = furpick::parse_hash(&self.rim_add_buf) {
                self.cfg.rim_ps = furpick::list_add(&self.cfg.rim_ps, h);
                self.apply_rim();
                self.rim_add_buf.clear();
            }
        }
        if ui.button("Verified shaders##rimcharr") {
            self.cfg.rim_ps = furpick::CHARR_PRESET.to_string();
            self.apply_rim();
        }
        ui.same_line();
        if ui.button("Same as fur list") {
            self.cfg.rim_ps = self.cfg.fur_ps.clone();
            self.apply_rim();
        }
        ui.same_line();
        if ui.button("Clear list##rimclear") {
            self.cfg.rim_ps.clear();
            self.apply_rim();
        }
        help(ui, "The default list is the verified charr skin. Other races and armour use other shaders: add them from the picker window of the Fur section (hashes), or use 'Same as fur list'.");
        ui.text_wrapped(format!("Rim mask: {}", furmask::rim_status_text()));
    }

    /// The picker window: the skinned draws of the last frame, one row per group. Ticking 'fur' edits fur_ps live;
    /// hovering or clicking a row paints only that group magenta in the scene.
    fn picker_window(&mut self, ui: &Ui) {
        let [dw, dh] = ui.io().display_size;
        let mut open = true;
        let rows = furpick::rows();
        let mut hover: Option<furpick::HighlightKey> = None;
        let shown = ui
            .window("Pick fur shaders")
            .size([furpick::picker_width(dw, dh), 0.0], Condition::FirstUseEver)
            .size_constraints([320.0, 120.0], [dw.max(320.0), dh.max(120.0)])
            .position([60.0 + furpick::window_width(dw, dh) + 20.0, 60.0], Condition::FirstUseEver)
            .opened(&mut open)
            .build(|| {
                help(ui, "Skinned draws of the last frame (vertex constant buffer 0 of 1 KB or more), one row per group. Hover or click a row: only that group turns magenta in the scene (head, hands, armour...). Tick 'fur' to make its pixel shader a fur shader. Save keeps the list.");
                ui.text_wrapped(format!("{} groups. {}", rows.len(), if rows.is_empty() { "Nothing yet: wait a frame, and make sure the character is on screen." } else { "" }));
                if ui.button("Clear highlight") {
                    self.picker_sel = None;
                }
                if let Some(k) = self.picker_sel {
                    ui.same_line();
                    ui.text(format!("selected {:08x}", k.ps));
                }
                // This imgui build has no table API: the classic columns do the job.
                {
                    ui.columns(7, "fur_groups", true);
                    for c in ["fur", "pixel shader", "vertex shader", "vcb", "draws", "idx or vtx / draw", "texture"] {
                        ui.text(c);
                        ui.next_column();
                    }
                    ui.separator();
                    for (i, r) in rows.iter().enumerate() {
                        let mut is_fur = furpick::list_has(&self.cfg.fur_ps, r.ps);
                        if ui.checkbox(format!("##fur{i}"), &mut is_fur) {
                            self.cfg.fur_ps = furpick::list_toggle(&self.cfg.fur_ps, r.ps).0;
                            self.apply_fur();
                        }
                        let mut hov = ui.is_item_hovered();
                        ui.next_column();
                        let sel = self.picker_sel == Some(r.key());
                        if ui.selectable_config(format!("{:08x}##row{i}", r.ps)).selected(sel).build() {
                            self.picker_sel = if sel { None } else { Some(r.key()) };
                        }
                        hov |= ui.is_item_hovered();
                        ui.next_column();
                        ui.text(r.vs.map_or("unknown".to_string(), |v| format!("{v:08x}")));
                        ui.next_column();
                        ui.text(format!("{}", r.vcb_max));
                        ui.next_column();
                        ui.text(format!("{}", r.draws));
                        ui.next_column();
                        let n = u64::from(r.draws.max(1));
                        ui.text(if r.indices > 0 { format!("{} idx", r.indices / n) } else { format!("{} vtx", r.vertices / n) });
                        ui.next_column();
                        ui.text(r.tex0.map_or("-".to_string(), |(w, h)| format!("{w}x{h}")));
                        ui.next_column();
                        if hov {
                            hover = Some(r.key());
                        }
                    }
                    ui.columns(1, "fur_groups_end", false);
                }
                ui.text_wrapped(format!("Fur mask: {}", furmask::status_text()));
            })
            .is_some();
        self.picker_shown = shown;
        furpick::set_highlight(if shown { hover.or(self.picker_sel) } else { None });
        if !open {
            self.picker_open = false;
            self.picker_sel = None;
        }
    }

    fn sec_photo(&mut self, ui: &Ui) {
        help(ui, "Saves the current frame at full resolution, with the mod's effects, into GW2Graphics\\photos: a PNG (sRGB, what an SDR screen would show), a Radiance .hdr (linear, 1.0 = paper white, values above 1.0 kept) and a .txt with every setting. Purely visual. Only one capture runs at a time; further requests wait in a queue.");
        if ui.button("Capture screenshot") {
            photo::request(photo::PhotoKind::Single);
        }
        ui.same_line();
        if ui.button("Compare with and without mod") {
            photo::request(photo::PhotoKind::Pair);
        }
        help(ui, "The comparison saves two photos (-mod and -vanilla); the screen flickers to the vanilla look for about 0.3 s, then your settings are restored.");
        let mut hide = photo::hide_ui();
        if ui.checkbox("Hide the UI (3D scene only)", &mut hide) {
            photo::set_hide_ui(hide);
            self.cfg.photo_hide_ui = hide;
        }
        help(ui, "Takes the 3D scene from the engine's pre-UI copy instead of the finished frame (single captures only; the comparison always includes the UI). If that copy is not seen within about 1.5 s the normal photo is taken. Save keeps this switch.");
        ui.text_wrapped(format!("Status: {}", photo::status_text()));
        ui.text_wrapped(format!("Hotkey: {} (config key photo_key, needs a restart)", self.cfg.photo_key));
    }

    fn sec_calib(&mut self, ui: &Ui) {
        let avail = output::calibration_available();
        help(ui, "Test patterns for the display, drawn by the mod's own last pass in absolute nits (the overlay cannot draw them: its colours stop at paper white). Pick a pattern, move this window aside, look at the picture, set the values here, then switch the pattern off. It also ends by itself after 30 s and when this window closes.");
        ui.text_wrapped(format!("Swapchain: {}", output::format_status()));
        if !avail {
            ui.text_colored(ui_col([1.0, 0.7, 0.3, 1.0]), "Unavailable: HDR output is not active (see Rule in Debug).");
        }
        let mut cur = calib::pattern() as i32;
        let mut changed = false;
        for (label, v) in [("Off", 0), ("1 Peak", 1), ("2 Paper white", 2), ("3 Black level", 3)] {
            changed |= ui.radio_button(label, &mut cur, v);
            if v < 3 {
                ui.same_line();
            }
        }
        if changed {
            let now = output::set_calibration_from(cur as u8, true);
            log!("overlay: calibration pattern {now}");
        }
        let p = calib::pattern();
        match p {
            calib::PEAK => {
                help(ui, "Eight blocks from 400 to 4000 nits. Raise the Peak slider until the last block you can still tell apart from the one next to it matches the peak; blocks above your display's real peak look the same. The value Windows reports for the display is shown below.");
            }
            calib::PAPER => {
                help(ui, "Left: a white UI swatch at exactly paper white. Middle: an 18 % grey card. Right: 5 % dark grey. Set the Paper white slider so the white swatch is comfortable (not glaring) in your room, while the grey card still looks like a mid grey.");
            }
            calib::BLACK => {
                help(ui, "Eight dim patches from 0.005 to 1 nit on black. Tell the guidance which is the dimmest you can still see. This is read-only advice: the mod never lifts blacks, so there is no setting to change.");
                let mut pick = (self.black_pick + 1) as usize;
                if ui.combo_simple_string("Dimmest visible patch", &mut pick, &["none", "0.005", "0.01", "0.02", "0.05", "0.1", "0.2", "0.5", "1"]) {
                    self.black_pick = pick as i32 - 1;
                }
                ui.text_wrapped(calib::black_guidance(if self.black_pick < 0 { None } else { Some(self.black_pick as usize) }));
            }
            _ => {}
        }
        if p == calib::PEAK || p == calib::PAPER {
            let (mut pw, mut peak) = (self.cfg.paper_white_nits, self.cfg.peak_nits);
            let mut apply = false;
            if p == calib::PAPER && ui.slider("Paper white (nits)", PAPER_WHITE_RANGE.0, PAPER_WHITE_RANGE.1, &mut pw) {
                self.cfg.paper_white_nits = clamp_levels(pw, peak, 1.0).0;
            }
            apply |= p == calib::PAPER && ui.is_item_deactivated_after_edit();
            if p == calib::PEAK {
                if ui.slider("Peak (nits)", PEAK_RANGE.0, PEAK_RANGE.1, &mut peak) {
                    self.cfg.peak_nits = clamp_levels(pw, peak, 1.0).1;
                }
                apply |= ui.is_item_deactivated_after_edit();
                match output::display_max_luminance() {
                    Some(d) => {
                        ui.text_wrapped(format!("Display reports {d:.0} nits (block {} of 8)", calib::peak_block_for(d) + 1));
                        if ui.button("Use display value") {
                            if let Some(v) = calib::peak_from_display(Some(d), PEAK_RANGE) {
                                self.cfg.peak_nits = v;
                                apply = true;
                            }
                        }
                    }
                    None => ui.text("The display does not report a peak."),
                }
            }
            if apply && output::mode() == OutputMode::ScRgb && self.cfg.enabled {
                output::set_levels(self.cfg.paper_white_nits, self.cfg.peak_nits);
            }
            help(ui, "Applies when you release the slider. Save keeps it.");
        }
        ui.separator();
        let mut idx = if self.cfg.hdr_format == crate::config::HdrFormat::Hdr10 { 1 } else { 0 };
        if ui.combo_simple_string("HDR format", &mut idx, &["scRGB float16 (default)", "HDR10 (experimental)"]) {
            self.cfg.hdr_format = if idx == 1 { crate::config::HdrFormat::Hdr10 } else { crate::config::HdrFormat::Scrgb };
        }
        help(ui, "Click Save, then restart the game. HDR10 is experimental and never used with ReShade or when Windows HDR is off; if a start with HDR10 fails to reach a stable picture it switches itself off (see docs/hdr10.md).");
    }

    /// The labels of the running calibration pattern, drawn on top (dim, in the output's encoding).
    fn calib_labels(&self, ui: &Ui) {
        let p = calib::pattern();
        if p == calib::OFF {
            return;
        }
        let [w, h] = ui.io().display_size;
        let dl = ui.get_foreground_draw_list();
        let col = ui_col([0.9, 0.9, 0.9, 1.0]);
        for i in 0..calib::cells(p) {
            if let (Some((x, y)), Some(t)) = (calib::label_pos(p, i), calib::label(p, i)) {
                let sz = ui.calc_text_size(&t);
                dl.add_text([x * w - sz[0] * 0.5, y * h], col, &t);
            }
        }
        let title = format!("Calibration: {} - move this window aside, switch the pattern off when done", calib::name(p));
        let sz = ui.calc_text_size(&title);
        dl.add_text([(w - sz[0]) * 0.5, h * 0.12], col, &title);
    }

    fn sec_debug(&mut self, ui: &Ui) {
        let mut level = resolve::debug_view() as usize;
        if ui.combo_simple_string("Debug view", &mut level, &["Off", "1 Boost / occlusion", "2 Fill", "3 Material classes", "4 Sun", "5 Reflectivity map"]) {
            self.cfg.debug_view = resolve::set_debug_view(level.min(resolve::DEBUG_VIEW_MAX as usize) as u8);
        }
        help(ui, "The same switch as the checkboxes below (the highest level wins) and the config key debug_view, which the dev hot reload applies from outside. Never saved.");
        let mut dbg = resolve::debug();
        if ui.checkbox("Debug view", &mut dbg) {
            resolve::set_debug(dbg);
        }
        help(ui, "Boosted pixels red, occluded pixels blue, halo sources green.");
        let mut dfill = resolve::debug_fill();
        if ui.checkbox("Debug fill", &mut dfill) {
            resolve::set_debug_fill(dfill);
        }
        help(ui, "Left half red, top right black.");
        let mut dmat = resolve::debug_materials();
        if ui.checkbox("Material classes", &mut dmat) {
            resolve::set_debug_materials(dmat);
        }
        help(ui, "Alpha class: low blue, mid green, high red.");
        let mut dsun = resolve::debug_sun();
        if ui.checkbox("Debug sun", &mut dsun) {
            resolve::set_debug_sun(dsun);
        }
        help(ui, "Sky cyan, shaft light yellow, magenta cross, log line.");
        if ui.button("Log depth stats") {
            depthstats::arm();
        }
        help(ui, "Writes a 'depthstats:' block to the log at the next frame (how the sky is encoded in the depth).");
        ui.separator();
        ui.text("Status");
        let mode = output::mode();
        // Wrapped: the reason can be long (it was cut off at the window edge before).
        ui.text_wrapped(format!("Classifier: {}", recorder::last_verdict()));
        ui.text(format!("Gate: {}", yes(recorder::gate_on())));
        ui.text(format!("Output: {mode:?}"));
        match output::display_max_luminance() {
            Some(d) => ui.text_wrapped(format!("Display peak: {d:.0} nits (reported by Windows); used peak: {:.0}", output::peak())),
            None => ui.text("Display peak: not reported"),
        }
        ui.text_wrapped(format!("Rule: {}", output::reason()));
        ui.text_wrapped(format!("Swapchain format: {}", output::format_status()));
        ui.text_wrapped(format!("Calibration: {}", calib::status_text(output::calibration_available(), crate::log::uptime_ms())));
        ui.text_wrapped(format!("Photo: {}", photo::status_text()));
        let fin = output::final_pass_status();
        let text = format!("Final pass: {}", fin.text());
        if fin.is_problem() {
            ui.text_colored(ui_col([1.0, 0.35, 0.3, 1.0]), text);
        } else {
            ui.text_wrapped(text);
        }
        ui.text(format!("Float upgrade: {} ({} targets)", if upgrade::active() { "active" } else { "off" }, d3d::upgraded_count()));
        ui.text(format!("Substitution: {}", yes(shaders::substitution_enabled())));
        ui.text_wrapped(format!("Data path: light-adjust inputs bound {} times", datapath::bind_count()));
        ui.text_wrapped(format!("AO: {}", ao::status_text()));
        ui.text_wrapped(format!("Bloom: {}", crate::bloomfix::status_text()));
        ui.text_wrapped(format!("Halo: {}", halo::status_text()));
        ui.text_wrapped(format!("Exposure: {}", crate::exposure::status_text()));
        ui.text_wrapped(format!("Local tone: {}", crate::exposure::status_local_text()));
        ui.text_wrapped(format!("Reflections: {}", ssr::status_text()));
        ui.text_wrapped(format!("Rim light: {}", furmask::rim_status_text()));
        ui.text_wrapped(format!("Sun shafts: {}", shafts::status_text()));
        ui.text_wrapped(format!("Contact shadows: {}", crate::contact::status_text()));
        ui.text_wrapped(format!("Aerial perspective: {}", crate::aerial::status_text()));
        ui.text_wrapped(format!("Depth stats: {}", depthstats::status_text()));
        ui.text_wrapped(format!("Frame time: {}", devshot::perf_status()));
        ui.text(format!("Copy format mismatches: {}", d3d::copy_mismatches()));
        ui.text_wrapped(recorder::counters_text(d3d::hooks()));
        ui.separator();
        ui.text("Dev (feedback flow, see docs/dev-flow.md)");
        if ui.button("Save screenshot now") {
            devshot::request_single();
        }
        ui.same_line();
        if ui.button("Save screenshot pair now") {
            devshot::request_pair();
        }
        let mut secs = devshot::capture_seconds() as i32;
        if ui.slider("Capture every (s, 0 = off)", 0, 120, &mut secs) {
            devshot::set_capture_seconds(secs.max(0) as u32);
            self.cfg.dev_capture_seconds = devshot::capture_seconds();
        }
        let mut cmp = devshot::compare();
        if ui.checkbox("Capture mod + vanilla pair", &mut cmp) {
            devshot::set_compare(cmp);
            self.cfg.dev_compare = cmp;
        }
        help(ui, "The screen flickers about 0.3 s per pair.");
        let mut hot = devshot::hot_reload();
        if ui.checkbox("Hot reload config.ini", &mut hot) {
            devshot::set_hot_reload(hot);
            self.cfg.dev_hot_reload = hot;
        }
        ui.text_wrapped(format!("Dev: {}", devshot::status()));
        if ui.button("Dump frame") {
            recorder::request_dump();
            // Also arms the per-draw capture of the next frame (draws-<unix>.log).
        }
        ui.text_wrapped(format!("Draw capture: {}", crate::drawdiag::status_text()));
        let d = recorder::dump_status();
        if !d.is_empty() {
            ui.text_wrapped(&d);
        }
    }

    fn footer(&mut self, ui: &Ui) {
        if ui.button("Save") {
            self.note = match self.cfg.save(&self.path) {
                Ok(()) => {
                    log!("overlay: config saved");
                    "Saved config.ini".to_string()
                }
                Err(e) => {
                    log!("overlay: could not save {}: {e}", self.path.display());
                    format!("Could not save: {e}")
                }
            };
        }
        if !self.note.is_empty() {
            ui.same_line();
            ui.text_wrapped(&self.note);
        }
        help(ui, &format!("Hotkey: {}", self.cfg.overlay_key));
    }
}

impl Overlay {
    /// Work that must happen every frame whether or not the panel is open: resolve, output, calibration, dev shots.
    pub fn per_frame(&mut self) {
        let _ = std::panic::catch_unwind(|| recorder::frame_end(d3d::hooks()));
        let _ = std::panic::catch_unwind(output::tick);
        let _ = std::panic::catch_unwind(output::frame_tick);
        // The calibration pattern's safety rules (30 s, overlay closed, HDR gone).
        let _ = std::panic::catch_unwind(|| {
            let p = calib::pattern();
            if p != calib::OFF {
                let open = calib::overlay_counts_as_open(VISIBLE.load(Ordering::Relaxed), calib::via_overlay());
                if let calib::Action::TurnOff(why) = calib::step(p, calib::since_ms(), crate::log::uptime_ms(), open, output::calibration_available()) {
                    output::set_calibration(0);
                    log!("overlay: calibration pattern off ({why})");
                }
            }
        });
        // Dev flow: the back buffer copy is issued here, before the overlay is drawn (so the shot has no overlay).
        devshot::on_frame();
        if let Some((old, new)) = devshot::take_reload() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (merged, d) = devshot::merge(&self.cfg, &old, &new);
                devshot::apply_live(&d.live, &merged);
                devshot::log_reload(&d);
                self.cfg = merged;
            }));
        }
    }

    /// Photo key, labels and the shader picker window. `shown`: the panel is on screen (the picker needs it).
    /// `window` draws the panel's window when the caller owns one.
    pub fn after_frame(&mut self, ui: &Ui, shown: bool, window: impl FnOnce(&mut Self, &Ui)) {
        self.poll_photo_key();
        self.calib_labels(ui);
        self.picker_shown = false;
        if shown {
            window(self, ui);
            if self.picker_open {
                self.picker_window(ui);
            }
        }
        // The picker collects draws only while its window is shown; closing it also clears the highlight.
        furpick::set_open(self.picker_shown);
    }
}

pub fn hdr10_active() -> bool {
    output::hdr10_active()
}

/// Panel waiting for the unified overlay to pick it up (when the DLL is not standalone).
pub static PANEL: std::sync::Mutex<Option<Overlay>> = std::sync::Mutex::new(None);

/// hudhook creates a dummy device and swapchain while installing; they must not be adopted as the game's.
pub fn while_hooking_present<R>(f: impl FnOnce() -> R) -> R {
    d3d::ignore_creations(true);
    let r = f();
    d3d::ignore_creations(false);
    r
}

impl ImguiRenderLoop for Overlay {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        ctx.set_ini_filename(None);
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        // hudhook sets the display size from the swapchain's buffers each frame: the most
        // reliable screen size (also follows resizes and works with ReShade).
        let io = ctx.io_mut();
        let [w, h] = io.display_size;
        d3d::hooks().set_screen(w as u32, h as u32);
        io.font_global_scale = (h / 1080.0).clamp(1.0, 2.0);
        // ImGui's own cursor is plain white: under HDR10 that would be 10000 nits, so it is not drawn there.
        io.mouse_draw_cursor = VISIBLE.load(Ordering::Relaxed) && !output::hdr10_active();
        self.map_style(ctx);
    }

    fn render(&mut self, ui: &mut Ui) {
        self.per_frame();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.poll_key();
            self.after_frame(ui, VISIBLE.load(Ordering::Relaxed), |s, ui| s.window(ui));
        }));
        if r.is_err() {
            VISIBLE.store(false, Ordering::Relaxed);
            furpick::set_open(false);
        }
    }

    fn message_filter(&self, _io: &Io) -> MessageFilter {
        if VISIBLE.load(Ordering::Relaxed) { MessageFilter::InputAll } else { MessageFilter::empty() }
    }
}

/// Installs the overlay. `cfg` is what the config file said at start (the window edits a copy);
/// `path` is where Save writes.
pub fn install(_module: usize, cfg: Config, path: PathBuf) {
    let (k, m) = parse_key(&cfg.overlay_key).unwrap_or_else(|| {
        log!("overlay: bad overlay_key '{}', using Shift+End", cfg.overlay_key);
        (0x23, MOD_SHIFT)
    });
    KEY.store(k, Ordering::Relaxed);
    MODS.store(m, Ordering::Relaxed);
    let (pk, pm) = parse_key(&cfg.photo_key).unwrap_or_else(|| {
        log!("overlay: bad photo_key '{}', using Ctrl+Shift+End", cfg.photo_key);
        (0x23, MOD_CTRL | MOD_SHIFT)
    });
    PHOTO_KEY.store(pk, Ordering::Relaxed);
    PHOTO_MODS.store(pm, Ordering::Relaxed);
    let key_text = cfg.overlay_key.clone();
    devshot::start(path.clone(), cfg.clone());
    *PANEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(Overlay::new(cfg, path));
    log!("overlay: panel ready for the unified overlay (key {key_text})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys() {
        assert_eq!(parse_key("Shift+End"), Some((0x23, MOD_SHIFT)));
        assert_eq!(parse_key("Insert"), Some((0x2D, 0)));
        assert_eq!(parse_key("ctrl+f9"), Some((0x78, MOD_CTRL)));
        assert_eq!(parse_key("Ctrl+Alt+K"), Some((0x4B, MOD_CTRL | MOD_ALT)));
        assert_eq!(parse_key("0x24"), Some((0x24, 0)));
        assert_eq!(parse_key("garbage"), None);
        assert_eq!(parse_key("Shift+"), None);
        assert_eq!(parse_key(""), None);
    }

    #[test]
    fn slider_values_are_clamped_to_the_caps() {
        assert_eq!(clamp_levels(1000.0, 10.0, 9.0), (400.0, 400.0, 2.0));
        assert_eq!(clamp_levels(10.0, 5000.0, -1.0), (80.0, 4000.0, 0.0));
        assert_eq!(clamp_levels(300.0, 200.0, 1.0).1, 400.0);
    }
}
