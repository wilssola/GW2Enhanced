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
use gw2enhanced_core::i18n::tr;
use gw2enhanced_core::trf;
use crate::output::OutputMode;
use crate::{ao, calib, d3d, datapath, depthstats, debugshot, furmask, furpick, gate, halo, hdr10, hdr_shaders, log, output, photo, recorder, resolve, shaders, shafts, ssr, upgrade};

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
    if b { tr("on", "ligado") } else { tr("off", "desligado") }
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

/// The name of a calibration pattern, for the on-screen title (`calib::name` is the log's).
fn calib_name(p: u8) -> &'static str {
    match p {
        calib::PEAK => tr("peak", "pico"),
        calib::PAPER => tr("paper white", "branco de referência"),
        calib::BLACK => tr("black level", "nível de preto"),
        _ => tr("off", "desligado"),
    }
}

/// The tooltip of a grading preset (the name stays as it is: it is the value of `grade_preset`).
fn preset_tooltip(p: &crate::grading::Preset) -> &'static str {
    let pt = match p.name {
        "Original" => "tudo neutro: a imagem como o jogo / o resto do mod a faz",
        "Warm cinematic" => "luz quente, sombras um pouco mais profundas, um pouco mais de contraste, cores mais calmas",
        "Cool" => "balanço de branco mais frio, um toque a mais de contraste, realces um pouco mais claros",
        "Vivid" => "cores mais ricas (principalmente as apagadas), um pouco mais de contraste",
        "Soft" => "contraste baixo, sombras mais claras, realces mais calmos, cores um pouco mais suaves",
        _ => p.what,
    };
    tr(p.what, pt)
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
        let i = fx.index();
        if ui.checkbox(trf!("{label} on##fx{i}", "{label} ativo##fx{i}"), &mut on) {
            self.cfg.toggles.0[fx.index()] = on;
            if crate::toggles::apply(&self.cfg.toggles) {
                hdr_shaders::set_bloom_strength(self.cfg.bloom_strength);
            }
        }
        if crate::toggles::master_off() {
            help(ui, tr(
            "(all effects are off by the master switch in General)",
            "(todos os efeitos estão desligados pela chave geral em Geral)",
        ));
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
        // One switch for the whole graphics mod (saved as `enabled`), first in the tab like the other features' switches:
        // off stops the effects AND the float targets, HDR shaders and resolve, so it is also the A/B against the engine.
        // The per-effect switches below are kept.
        let mut on = self.cfg.enabled && !crate::toggles::master_off();
        if ui.checkbox(tr("Graphics on", "Gráficos ligados"), &mut on) {
            self.set_graphics_on(on);
        }
        help(ui, tr(
            "Off: the game's original look, from the next frame. With HDR output the final conversion stays on (plain SDR brightness); a full revert of HDR output needs a restart.",
            "Desligados: o visual original do jogo, já no próximo quadro. Com saída HDR, a conversão final continua ligada (brilho SDR simples); para desfazer a saída HDR por completo é preciso reiniciar.",
        ));
        help(ui, &trf!("Effects: {}", "Efeitos: {}", crate::toggles::status_text()));
        ui.separator();
        if section(ui, tr("General / HDR###sec-general", "Geral / HDR###sec-general"), true) {
            self.sec_general(ui);
        }
        if section(ui, tr("Bloom + highlight###sec-bloom", "Bloom + realce###sec-bloom"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Bloom, "Bloom");
            self.fx_toggle(ui, crate::toggles::Fx::Highlight, tr("Highlight boost", "Realce de brilhos"));
            self.sec_bloom(ui);
        }
        if section(ui, tr("Ambient occlusion (AO)###sec-ao", "Oclusão de ambiente (AO)###sec-ao"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Ao, "AO");
            self.sec_ao(ui);
        }
        if section(ui, tr("Halo###sec-halo", "Halo###sec-halo"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Halo, "Halo");
            self.sec_halo(ui);
        }
        if section(ui, tr("Reflections (SSR)###sec-ssr", "Reflexos (SSR)###sec-ssr"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Ssr, tr("Reflections", "Reflexos"));
            self.sec_ssr(ui);
        }
        if section(ui, tr("Sun shafts###sec-shafts", "Raios de sol###sec-shafts"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Shafts, tr("Sun shafts", "Raios de sol"));
            self.sec_shafts(ui);
        }
        if section(ui, tr("Contact shadows###sec-contact", "Sombras de contato###sec-contact"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Contact, tr("Contact shadows", "Sombras de contato"));
            self.sec_contact(ui);
        }
        if section(ui, tr("Aerial perspective###sec-aerial", "Perspectiva atmosférica###sec-aerial"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Aerial, tr("Aerial perspective", "Perspectiva atmosférica"));
            self.sec_aerial(ui);
        }
        if section(ui, tr("Exposure & tone###sec-exposure", "Exposição e tons###sec-exposure"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Exposure, tr("Auto exposure", "Exposição automática"));
            self.fx_toggle(ui, crate::toggles::Fx::LocalTone, tr("Local tone mapping", "Mapeamento de tons local"));
            self.sec_exposure(ui);
        }
        if section(ui, tr("Lens effects###sec-lens", "Efeitos de lente###sec-lens"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Lens, tr("Lens effects", "Efeitos de lente"));
            self.sec_lens(ui);
        }
        if section(ui, tr("Color grading###sec-grade", "Gradação de cores###sec-grade"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Grading, tr("Colour grading / LUT", "Gradação de cores / LUT"));
            self.sec_grade(ui);
        }
        if section(ui, tr("Anti-aliasing & filtering###sec-aa", "Anti-aliasing e filtragem###sec-aa"), false) {
            self.sec_aa(ui);
        }
        if section(ui, tr("Fur###sec-fur", "Pelagem###sec-fur"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::FurLook, tr("Fur look", "Visual da pelagem"));
            self.fx_toggle(ui, crate::toggles::Fx::Shells, tr("Shell fur", "Pelagem em camadas"));
            self.sec_fur(ui);
        }
        if section(ui, tr("Rim light & skin###sec-rim", "Luz de contorno e pele###sec-rim"), false) {
            self.fx_toggle(ui, crate::toggles::Fx::Rim, tr("Rim light", "Luz de contorno"));
            self.sec_rim(ui);
        }
        if section(ui, tr("Photo mode###sec-photo", "Modo foto###sec-photo"), false) {
            self.sec_photo(ui);
        }
        if section(ui, tr("HDR calibration###sec-calib", "Calibração de HDR###sec-calib"), false) {
            self.sec_calib(ui);
        }
        if section(ui, tr("Debug###sec-debug", "Depuração###sec-debug"), false) {
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
        // The switch is kept right away (like the other features' switches); the rest of the file only changes with Save.
        let mut saved = Config::load(&self.path);
        saved.enabled = on;
        if let Err(e) = saved.save(&self.path) {
            log!("overlay: could not save enabled={on} to {}: {e}", self.path.display());
        }
    }

    fn sec_general(&mut self, ui: &Ui) {
        // v18: the diagnostic A/B of the float render-target upgrade (applies at the next game start; Save writes it)
        let mut up = self.cfg.hdr_upgrade;
        if ui.checkbox(tr("Float scene upgrade (restart the game; off = engine-identical, nothing runs)", "Cena em ponto flutuante (reinicie o jogo; desligado = igual ao motor, nada roda)"), &mut up) {
            self.cfg.hdr_upgrade = up;
        }
        if crate::upgrade::forced_off() {
            help(ui, tr(
            "Running with hdr_upgrade=0: the engine's own formats, no substitute, no effect (diagnostic).",
            "Rodando com hdr_upgrade=0: os formatos do próprio motor, sem substituto, sem efeito (diagnóstico).",
        ));
        }
        // v18: the clamp emulation (live)
        let mut cl = self.cfg.hdr_clamp;
        if ui.checkbox(tr("8-bit clamp for engine shaders (fixes black volumes; off = A/B)", "Limite de 8 bits nos shaders do motor (corrige volumes pretos; desligado = A/B)"), &mut cl) {
            self.cfg.hdr_clamp = cl;
            crate::clampemu::set_enabled(cl);
        }
        help(ui, &trf!("Clamp emulation: {}", "Emulação do limite: {}", crate::clampemu::status_text()));
        let mut idx = match self.cfg.hdr_output {
            HdrOutput::Auto => 0,
            HdrOutput::On => 1,
            HdrOutput::Off => 2,
        };
        if ui.combo_simple_string(tr("HDR output", "Saída HDR"), &mut idx, &[tr("Auto", "Automático"), tr("On", "Ligado"), tr("Off", "Desligado")]) {
            self.cfg.hdr_output = [HdrOutput::Auto, HdrOutput::On, HdrOutput::Off][idx.min(2)];
        }
        help(ui, tr(
            "Click Save, then restart the game: the swapchain is created once at start.",
            "Clique em Salvar e reinicie o jogo: a swapchain é criada uma vez, ao iniciar.",
        ));
        let (mut pw, mut peak) = (self.cfg.paper_white_nits, self.cfg.peak_nits);
        // The values follow the slider while dragging, but are applied once, when the edit is
        // committed (mouse released / Enter): applying re-compiles a shader on the render thread.
        let c1 = ui.slider(tr("Paper white (nits)", "Branco de referência (nits)"), PAPER_WHITE_RANGE.0, PAPER_WHITE_RANGE.1, &mut pw);
        let k1 = ui.is_item_deactivated_after_edit();
        let c2 = ui.slider(tr("Peak (nits)", "Pico (nits)"), PEAK_RANGE.0, PEAK_RANGE.1, &mut peak);
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
        help(ui, tr(
            "Paper white and peak act in HDR output only. Values apply when you release the slider.",
            "Branco de referência e pico só valem com saída HDR. Os valores são aplicados quando você solta o controle.",
        ));
        // Black level: the SDR decode gamma of the final pass (0 = the piecewise sRGB curve Windows uses).
        let mut sg = self.cfg.sdr_gamma;
        let cg = ui.slider(tr("SDR gamma (black level, 0 = sRGB)", "Gama SDR (nível de preto, 0 = sRGB)"), 0.0, output::SDR_GAMMA_RANGE.1, &mut sg);
        let kg = ui.is_item_deactivated_after_edit();
        if cg {
            self.cfg.sdr_gamma = output::clamp_sdr_gamma(sg);
        }
        if kg {
            self.cfg.sdr_gamma = output::set_sdr_gamma(self.cfg.sdr_gamma);
        }
        help(ui, tr(
            "2.2 = like an SDR monitor (deep shadows, default); 2.4 = deeper; 0 = the piecewise sRGB curve Windows uses for SDR content (lifted, greyer blacks). HDR output only.",
            "2.2 = como um monitor SDR (sombras profundas, padrão); 2.4 = mais profundas; 0 = a curva sRGB por partes que o Windows usa para conteúdo SDR (pretos mais claros, acinzentados). Só com saída HDR.",
        ));
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
        help(ui, tr(
            "Bloom applies when you release the slider.",
            "O bloom é aplicado quando você solta o controle.",
        ));
        let mut hb = self.cfg.highlight_boost;
        if ui.slider(tr("Highlight boost", "Realce de brilhos"), 1.0, 4.0, &mut hb) {
            self.cfg.highlight_boost = hb.clamp(1.0, 4.0);
            resolve::set_highlight(self.cfg.highlight_boost);
        }
        help(ui, tr(
            "Bright pixels (largest channel from 0.4 of SDR white, full from 0.9) are multiplied up to the display headroom at 4; lights get the whole gain, big bright surfaces 30 % of it. 1 = no boost. HDR only. Applies live.",
            "Pixels claros (maior canal a partir de 0.4 do branco SDR, total a partir de 0.9) são multiplicados até a margem da tela em 4; luzes recebem o ganho inteiro, superfícies grandes e claras 30 % dele. 1 = sem realce. Só HDR. Aplica na hora.",
        ));
        ui.text_wrapped(trf!("Highlight: {}", "Realce: {}", crate::datapath::highlight_status_text()));
    }

    fn sec_ao(&mut self, ui: &Ui) {
        let (mut aos, mut aor) = (self.cfg.ao_strength, self.cfg.ao_radius);
        // AO values go to atomics read by the render thread; the shaders are never recompiled.
        if ui.slider(tr("AO strength", "Intensidade do AO"), ao::STRENGTH_RANGE.0, ao::STRENGTH_RANGE.1, &mut aos) {
            self.cfg.ao_strength = ao::set_strength(aos);
        }
        if ui.slider(tr("AO radius", "Raio do AO"), ao::RADIUS_RANGE.0, ao::RADIUS_RANGE.1, &mut aor) {
            self.cfg.ao_radius = ao::set_radius(aor);
        }
        help(ui, tr(
            "AO strength 0 turns the pass off. Applies live.",
            "Intensidade 0 desliga o AO. Aplica na hora.",
        ));
    }

    fn sec_halo(&mut self, ui: &Ui) {
        let (mut hs, mut hr) = (self.cfg.halo_strength, self.cfg.halo_radius);
        // Halo values go to atomics read by the render thread; the shaders are never recompiled.
        if ui.slider(tr("Halo strength", "Intensidade do halo"), halo::STRENGTH_RANGE.0, halo::STRENGTH_RANGE.1, &mut hs) {
            self.cfg.halo_strength = halo::set_strength(hs);
        }
        if ui.slider(tr("Halo radius", "Raio do halo"), halo::RADIUS_RANGE.0, halo::RADIUS_RANGE.1, &mut hr) {
            self.cfg.halo_radius = halo::set_radius(hr);
        }
        help(ui, tr(
            "Halo strength 0 turns the whole feature off (no passes). Applies live.",
            "Intensidade 0 desliga o halo por completo (nenhuma passada). Aplica na hora.",
        ));
    }

    fn sec_ssr(&mut self, ui: &Ui) {
        let (mut ss, mut sr) = (self.cfg.ssr_strength, self.cfg.ssr_range);
        if ui.slider(tr("Reflection strength", "Intensidade dos reflexos"), ssr::STRENGTH_RANGE.0, ssr::STRENGTH_RANGE.1, &mut ss) {
            self.cfg.ssr_strength = ssr::set_strength(ss);
        }
        if ui.slider(tr("Reflection range", "Alcance dos reflexos"), ssr::RANGE_RANGE.0, ssr::RANGE_RANGE.1, &mut sr) {
            self.cfg.ssr_range = ssr::set_range(sr);
        }
        help(ui, tr(
            "Reflection strength 0 turns the passes off. Applies live.",
            "Intensidade 0 desliga as passadas. Aplica na hora.",
        ));
        let (mut wb, mut wd) = (self.cfg.ssr_water_boost, self.cfg.water_depth_look);
        if ui.slider(tr("Water / glass boost", "Reforço em água / vidro"), ssr::WATER_RANGE.0, ssr::WATER_RANGE.1, &mut wb) {
            self.cfg.ssr_water_boost = ssr::set_water_boost(wb);
        }
        if ui.slider(tr("Water depth look", "Aparência de profundidade da água"), ssr::DEPTH_LOOK_RANGE.0, ssr::DEPTH_LOOK_RANGE.1, &mut wd) {
            self.cfg.water_depth_look = ssr::set_depth_look(wd);
        }
        help(ui, tr(
            "Only on surfaces the score calls water or glass: Fresnel curve (at most 0.85), water tint, sky instead of a black band at the screen edge, sharp mirror on calm water / soft on ripples. Boost 0 = the old reflections exactly. Depth look darkens such water by up to 15 % where the reflection is strong (needs the boost).",
            "Só nas superfícies que a pontuação considera água ou vidro: curva de Fresnel (no máximo 0.85), tom de água, céu em vez de uma faixa preta na borda da tela, espelho nítido em água calma / suave em ondulações. Reforço 0 = exatamente os reflexos antigos. A profundidade escurece essa água em até 15 % onde o reflexo é forte (precisa do reforço).",
        ));
        let mut sm = self.cfg.ssr_smart;
        if ui.slider(tr("Material score amount", "Peso da pontuação de material"), ssr::SMART_RANGE.0, ssr::SMART_RANGE.1, &mut sm) {
            self.cfg.ssr_smart = ssr::set_smart(sm);
        }
        let (mut sh, mut sf) = (self.cfg.ssr_hue_filter, self.cfg.ssr_flatness);
        if ui.slider(tr("Hue filter (green, earth)", "Filtro de matiz (verde, terra)"), ssr::HUE_RANGE.0, ssr::HUE_RANGE.1, &mut sh) {
            self.cfg.ssr_hue_filter = ssr::set_hue_filter(sh);
        }
        if ui.slider(tr("Flatness (varying normals)", "Planura (normais variando)"), ssr::FLAT_RANGE.0, ssr::FLAT_RANGE.1, &mut sf) {
            self.cfg.ssr_flatness = ssr::set_flatness(sf);
        }
        let mut mc = self.cfg.ssr_matte_cut;
        if ui.slider(tr("Matte cut (dry ground)", "Corte fosco (chão seco)"), ssr::MATTE_RANGE.0, ssr::MATTE_RANGE.1, &mut mc) {
            self.cfg.ssr_matte_cut = ssr::set_matte_cut(mc);
        }
        help(ui, tr(
            "Matte cut: no gloss buffer exists, so moonlight, fog and shade make dry dirt look like water or polished stone. 0.8 (default) keeps reflections only where the surface still looks like water / glass / ice under the frame's own light; 1 = strictest; lower (0.3..0.5) brings grey polished floors back. 0 = the old score. Applies live.",
            "Corte fosco: não existe buffer de brilho, então luar, neblina e sombra fazem terra seca parecer água ou pedra polida. 0.8 (padrão) mantém reflexos só onde a superfície ainda parece água / vidro / gelo sob a luz do próprio quadro; 1 = mais rigoroso; valores menores (0.3..0.5) trazem de volta pisos cinza polidos. 0 = a pontuação antiga. Aplica na hora.",
        ));
        let mut cls = self.cfg.ssr_classes;
        let mut changed = false;
        for (bit, label) in [
            (ssr::CLASS_LOW, tr("Reflect on: low (blue)", "Refletir em: baixa (azul)")),
            (ssr::CLASS_MID, tr("Reflect on: mid (green)", "Refletir em: média (verde)")),
            (ssr::CLASS_HIGH, tr("Reflect on: high (red)", "Refletir em: alta (vermelho)")),
        ] {
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
        if ui.checkbox(tr("Reflectivity map", "Mapa de refletividade"), &mut drefl) {
            resolve::set_debug_reflectivity(drefl);
        }
        ui.text_colored(ui_col([0.35, 0.7, 1.0, 1.0]), tr("blue / cyan = reflective", "azul / ciano = reflete"));
        ui.text_colored(ui_col([0.6, 0.6, 0.6, 1.0]), tr("black = not reflective", "preto = não reflete"));
        help(ui, tr(
            "The map shows what the detector picks (blue 0 .. red 1). 'Material classes' in Debug shows the alpha classes. Save to keep.",
            "O mapa mostra o que o detector escolhe (azul 0 .. vermelho 1). 'Classes de material' em Depuração mostra as classes de alfa. Salve para manter.",
        ));
    }

    fn sec_shafts(&mut self, ui: &Ui) {
        let (mut shs, mut shl) = (self.cfg.shafts_strength, self.cfg.shafts_length);
        if ui.slider(tr("Sun shafts", "Raios de sol"), shafts::STRENGTH_RANGE.0, shafts::STRENGTH_RANGE.1, &mut shs) {
            self.cfg.shafts_strength = shafts::set_strength(shs);
        }
        if ui.slider(tr("Shaft length", "Comprimento dos raios"), shafts::LENGTH_RANGE.0, shafts::LENGTH_RANGE.1, &mut shl) {
            self.cfg.shafts_length = shafts::set_length(shl);
        }
        help(ui, tr(
            "Sun shafts 0 turns the feature off. Applies live. Debug view marks the sun with a cross.",
            "Raios de sol 0 desliga o efeito. Aplica na hora. A visão de depuração marca o sol com uma cruz.",
        ));
    }

    fn sec_contact(&mut self, ui: &Ui) {
        use crate::contact as ct;
        let (mut s, mut l) = (self.cfg.contact_strength, self.cfg.contact_length);
        // Atomics read by the render thread; nothing is recompiled.
        if ui.slider(tr("Contact shadow strength", "Intensidade das sombras de contato"), ct::STRENGTH_RANGE.0, ct::STRENGTH_RANGE.1, &mut s) {
            self.cfg.contact_strength = ct::set_strength(s);
        }
        if ui.slider(tr("Contact shadow length", "Comprimento das sombras de contato"), ct::LENGTH_RANGE.0, ct::LENGTH_RANGE.1, &mut l) {
            self.cfg.contact_length = ct::set_length(l);
        }
        let mut fixed = self.cfg.contact_sun == "fixed";
        if ui.checkbox(tr("Fixed light direction (ignore the sun estimate)", "Direção de luz fixa (ignora a estimativa do sol)"), &mut fixed) {
            self.cfg.contact_sun = ct::set_mode(if fixed { "fixed" } else { "auto" }).to_string();
        }
        help(ui, tr(
            "Contact shadows 0 turns the feature off. A short ray march in the depth buffer towards the light; never under water, in fog, on lights or the sky. Applies live; Save keeps it.",
            "Sombras de contato 0 desliga o efeito. Um traçado curto de raio no buffer de profundidade em direção à luz; nunca debaixo d'água, na neblina, em luzes ou no céu. Aplica na hora; Salvar mantém.",
        ));
        ui.text_wrapped(trf!("Status: {}", "Situação: {}", ct::status_text()));
    }

    fn sec_aerial(&mut self, ui: &Ui) {
        use crate::aerial as ae;
        let (mut s, mut d) = (self.cfg.aerial_strength, self.cfg.aerial_distance);
        if ui.slider(tr("Aerial perspective", "Perspectiva atmosférica"), ae::STRENGTH_RANGE.0, ae::STRENGTH_RANGE.1, &mut s) {
            self.cfg.aerial_strength = ae::set_strength(s);
        }
        if ui.slider(tr("Half-strength distance (m)", "Distância de meia intensidade (m)"), ae::DISTANCE_RANGE.0, 2000.0, &mut d) {
            self.cfg.aerial_distance = ae::set_distance(d);
        }
        let mut u = self.cfg.depth_unit;
        if ui.slider(tr("Depth unit (m at the far plane)", "Unidade de profundidade (m no plano distante)"), ae::UNIT_RANGE.0, 5000.0, &mut u) {
            self.cfg.depth_unit = ae::set_unit(u);
        }
        help(ui, tr(
            "Aerial perspective 0 turns the feature off. A subtle haze toward the game's own fog colour on pixels farther than 15 m (at most 35 %); never on the sky, lights, in dense fog or under water. The engine's depth is normalised (1.0 = far plane = sky): the depth unit is how many metres 1.0 is (1000 = default, measured; also scales the contact shadow length). Applies live; Save keeps it.",
            "Perspectiva atmosférica 0 desliga o efeito. Uma névoa sutil na cor da neblina do próprio jogo em pixels a mais de 15 m (no máximo 35 %); nunca no céu, em luzes, em neblina densa ou debaixo d'água. A profundidade do motor é normalizada (1.0 = plano distante = céu): a unidade de profundidade diz quantos metros vale 1.0 (1000 = padrão, medido; também ajusta o comprimento das sombras de contato). Aplica na hora; Salvar mantém.",
        ));
        ui.text_wrapped(trf!("Status: {}", "Situação: {}", ae::status_text()));
    }

    fn sec_exposure(&mut self, ui: &Ui) {
        use crate::exposure as ex;
        let (mut s, mut t, mut r) = (self.cfg.autoexp_strength, self.cfg.autoexp_target, self.cfg.autoexp_range);
        // All values go to atomics read by the render thread; nothing is recompiled.
        if ui.slider(tr("Auto exposure", "Exposição automática"), ex::STRENGTH_RANGE.0, ex::STRENGTH_RANGE.1, &mut s) {
            self.cfg.autoexp_strength = ex::set_strength(s);
        }
        if ui.slider(tr("Target mid-grey", "Cinza médio alvo"), ex::TARGET_RANGE.0, ex::TARGET_RANGE.1, &mut t) {
            self.cfg.autoexp_target = ex::set_target(t);
        }
        if ui.slider(tr("Max change (EV)", "Variação máxima (EV)"), ex::RANGE_RANGE.0, ex::RANGE_RANGE.1, &mut r) {
            self.cfg.autoexp_range = ex::set_range(r);
        }
        help(ui, tr(
            "Auto exposure 0 = off. Eye adaptation of the 3D scene (not the UI) towards the target; the change is limited to the max EV, brightening to +1 EV. Applies live.",
            "Exposição automática 0 = desligada. Adaptação do olho na cena 3D (não na interface) em direção ao alvo; a variação é limitada ao EV máximo, clareando até +1 EV. Aplica na hora.",
        ));
        let (mut up, mut down) = (self.cfg.adapt_up, self.cfg.adapt_down);
        if ui.slider(tr("Adapt to brighter (s)", "Adaptação ao claro (s)"), ex::ADAPT_RANGE.0, 5.0, &mut up) {
            self.cfg.adapt_up = ex::set_adapt_up(up);
        }
        if ui.slider(tr("Adapt to darker (s)", "Adaptação ao escuro (s)"), ex::ADAPT_RANGE.0, 5.0, &mut down) {
            self.cfg.adapt_down = ex::set_adapt_down(down);
        }
        help(ui, tr(
            "Time constants: about 95 % of a change after three times the value. The eye is slower going into the dark.",
            "Constantes de tempo: cerca de 95 % da mudança depois de três vezes o valor. O olho é mais lento indo para o escuro.",
        ));
        let mut lt = self.cfg.localtone_strength;
        if ui.slider(tr("Local tone mapping", "Mapeamento de tons local"), ex::LOCAL_RANGE.0, ex::LOCAL_RANGE.1, &mut lt) {
            self.cfg.localtone_strength = ex::set_local_strength(lt);
        }
        help(ui, tr(
            "0 = off. Compresses big bright areas (sand, snow, white fur) without flattening their detail, edge-aware (no halos). Lights keep their energy. Applies live.",
            "0 = desligado. Comprime áreas grandes e claras (areia, neve, pelo branco) sem achatar os detalhes, respeitando as bordas (sem halos). As luzes mantêm a energia. Aplica na hora.",
        ));
        ui.text_wrapped(trf!("Auto exposure: {}", "Exposição automática: {}", ex::status_text()));
        ui.text_wrapped(trf!("Local tone: {}", "Tons locais: {}", ex::status_local_text()));
    }

    fn sec_lens(&mut self, ui: &Ui) {
        use crate::lens as ln;
        let (mut s, mut t, mut g, mut l) = (self.cfg.lens_strength, self.cfg.lens_threshold, self.cfg.lens_ghost, self.cfg.lens_streak_length);
        // Atomics read by the render thread; nothing is recompiled.
        if ui.slider(tr("Streak / glare strength", "Intensidade dos riscos / ofuscamento"), ln::STRENGTH_RANGE.0, ln::STRENGTH_RANGE.1, &mut s) {
            self.cfg.lens_strength = ln::set_strength(s);
        }
        if ui.slider(tr("Threshold (x paper white)", "Limiar (x branco de referência)"), ln::THRESHOLD_RANGE.0, ln::THRESHOLD_RANGE.1, &mut t) {
            self.cfg.lens_threshold = ln::set_threshold(t);
        }
        if ui.slider(tr("Ghost flares", "Reflexos fantasma"), ln::GHOST_RANGE.0, ln::GHOST_RANGE.1, &mut g) {
            self.cfg.lens_ghost = ln::set_ghost(g);
        }
        if ui.slider(tr("Streak length", "Comprimento dos riscos"), ln::LENGTH_RANGE.0, ln::LENGTH_RANGE.1, &mut l) {
            self.cfg.lens_streak_length = ln::set_streak_length(l);
        }
        let (mut z, mut c) = (self.cfg.lens_max_size, self.cfg.lens_color_reject);
        if ui.slider(tr("Largest source size (share of height)", "Maior fonte (fração da altura)"), ln::MAX_SIZE_RANGE.0, ln::MAX_SIZE_RANGE.1, &mut z) {
            self.cfg.lens_max_size = ln::set_max_size(z);
        }
        if ui.slider(tr("Reject coloured glows", "Rejeitar brilhos coloridos"), ln::COLOR_REJECT_RANGE.0, ln::COLOR_REJECT_RANGE.1, &mut c) {
            self.cfg.lens_color_reject = ln::set_color_reject(c);
        }
        help(ui, tr(
            "Only compact light sources (fire, lamps, candles, the sun) get a flare: a bright area bigger than the largest source size (a glowing creature, a bright wall) gets none, 0 = no size test. Saturated blue, cyan, violet or green glows must be brighter (up to 1 + 4 x the reject value times the threshold); white and warm lights are not affected.",
            "Só fontes de luz compactas (fogo, lâmpadas, velas, o sol) ganham reflexo: uma área clara maior que a maior fonte (uma criatura brilhando, uma parede clara) não ganha nenhum, 0 = sem teste de tamanho. Brilhos azuis, ciano, violeta ou verdes saturados precisam ser mais fortes para dar reflexo; 0 = cor ignorada.",
        ));
        help(ui, tr(
            "Strength 0 turns the whole feature off (no passes). Horizontal streaks and a soft glare from HDR highlights above the threshold (measured before the highlight boost, so white sand and snow never trigger); ghost flares only for very bright point sources (2x the threshold), 0 = no ghosts. Warm white, never blue; adds at most 0.6 of paper white. HDR output only. Applies live; Save keeps it.",
            "Intensidade 0 desliga o efeito por completo (nenhuma passada). Riscos horizontais e um ofuscamento suave dos brilhos HDR acima do limiar (medido antes do realce, então areia branca e neve nunca disparam); reflexos fantasma só para fontes pontuais muito fortes (2x o limiar), 0 = sem fantasmas. Branco quente, nunca azul; soma no máximo 0.6 do branco de referência. Só com saída HDR. Aplica na hora; Salvar mantém.",
        ));
        ui.text_wrapped(trf!("Status: {}", "Situação: {}", crate::look::status_lens()));
    }

    fn apply_grade(&mut self) {
        let g = crate::grading::set(self.cfg.grade());
        self.cfg.set_grade(&g);
    }

    fn sec_grade(&mut self, ui: &Ui) {
        use crate::grading as gr;
        ui.text(tr("Presets", "Predefinições"));
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
                ui.tooltip_text(preset_tooltip(p));
            }
        }
        if !self.cfg.grade_preset.is_empty() {
            help(ui, &trf!(
                "Last preset: {} (the sliders below may have been moved since). Save writes grade_preset and every slider.",
                "Última predefinição: {} (os controles abaixo podem ter sido mexidos depois). Salvar grava grade_preset e todos os controles.",
                self.cfg.grade_preset
            ));
        }
        let mut g = self.cfg.grade();
        let mut changed = false;
        changed |= ui.slider(tr("Saturation", "Saturação"), gr::SATURATION_RANGE.0, gr::SATURATION_RANGE.1, &mut g.saturation);
        changed |= ui.slider(tr("Contrast", "Contraste"), gr::CONTRAST_RANGE.0, gr::CONTRAST_RANGE.1, &mut g.contrast);
        changed |= ui.slider(tr("Temperature (warm +)", "Temperatura (quente +)"), gr::TEMPERATURE_RANGE.0, gr::TEMPERATURE_RANGE.1, &mut g.temperature);
        changed |= ui.slider(tr("Tint (magenta +)", "Matiz (magenta +)"), gr::TINT_RANGE.0, gr::TINT_RANGE.1, &mut g.tint);
        changed |= ui.slider(tr("Vibrance", "Vibração"), gr::VIBRANCE_RANGE.0, gr::VIBRANCE_RANGE.1, &mut g.vibrance);
        changed |= ui.slider(tr("Shadows", "Sombras"), gr::SHADOWS_RANGE.0, gr::SHADOWS_RANGE.1, &mut g.shadows);
        changed |= ui.slider(tr("Highlights", "Realces"), gr::HIGHLIGHTS_RANGE.0, gr::HIGHLIGHTS_RANGE.1, &mut g.highlights);
        if changed {
            self.cfg.set_grade(&g);
            self.apply_grade();
        }
        help(ui, tr(
            "All neutral = off (nothing runs). Applied to the 3D scene before the UI in linear light; contrast pivots at mid-grey 0.18 and stays linear above 1.0, so HDR highlights keep their hue and are not clipped. Applies live; Save keeps it.",
            "Tudo neutro = desligado (nada roda). Aplicado à cena 3D antes da interface, em luz linear; o contraste gira em torno do cinza médio 0.18 e fica linear acima de 1.0, então os brilhos HDR mantêm a cor e não são cortados. Aplica na hora; Salvar mantém.",
        ));
        let mut ls = self.cfg.grade_lut_strength;
        if ui.slider(tr("LUT strength", "Intensidade da LUT"), gr::LUT_STRENGTH_RANGE.0, gr::LUT_STRENGTH_RANGE.1, &mut ls) {
            self.cfg.grade_lut_strength = gr::set_lut_strength(ls);
        }
        if matches!(gr::lut_state(), gr::LutState::Off) {
            ui.text_colored(ui_col([1.0, 0.7, 0.3, 1.0]), tr("No LUT loaded: the LUT strength does nothing. Pick one below.", "Nenhuma LUT carregada: a intensidade da LUT não faz nada. Escolha uma abaixo."));
        }
        self.lut_picker(ui);
        ui.text_wrapped(trf!(
            "LUT file (grade_lut in config.ini): {}",
            "Arquivo da LUT (grade_lut no config.ini): {}",
            if self.cfg.grade_lut.is_empty() { tr("none", "nenhum") } else { self.cfg.grade_lut.as_str() }
        ));
        ui.text_wrapped(trf!("LUT status: {}", "Situação da LUT: {}", gr::lut_status_text()));
        ui.text_wrapped(trf!("Status: {}", "Situação: {}", crate::look::status_grade()));
    }

    /// LUT selector: the `.cube` files of GW2Graphics\luts (listed when the section opens and on Refresh, not every frame)
    /// and a button that writes the four example LUTs there (never overwriting). Picking one loads it live (background thread).
    fn lut_picker(&mut self, ui: &Ui) {
        use crate::grading as gr;
        use std::sync::Mutex;
        static LIST: Mutex<Option<Vec<String>>> = Mutex::new(None);
        static NOTE: Mutex<String> = Mutex::new(String::new());
        let Some(dir) = gr::dir() else {
            ui.text_wrapped(tr("LUT folder: not known yet", "Pasta das LUTs: ainda desconhecida"));
            return;
        };
        let mut list = LIST.lock().unwrap_or_else(|e| e.into_inner());
        if list.is_none() {
            *list = Some(gr::list_luts_in(&dir));
        }
        let files = list.clone().unwrap_or_default();
        let mut items: Vec<String> = vec![tr("(none)", "(nenhuma)").to_string()];
        items.extend(files.iter().cloned());
        let cur = self.cfg.grade_lut.trim().replace('\\', "/");
        let mut idx = files.iter().position(|f| f.eq_ignore_ascii_case(&cur)).map_or(0, |i| i + 1);
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        if ui.combo_simple_string(tr("LUT (GW2Graphics\\luts)", "LUT (GW2Graphics\\luts)"), &mut idx, &refs) {
            self.cfg.grade_lut = if idx == 0 { String::new() } else { files[idx - 1].clone() };
            gr::set_lut(&self.cfg.grade_lut);
        }
        if ui.button(tr("Refresh list", "Atualizar lista")) {
            *list = Some(gr::list_luts_in(&dir));
        }
        ui.same_line();
        if ui.button(tr("Create example LUTs", "Criar LUTs de exemplo")) {
            let msg = match gr::write_example_luts_in(&dir) {
                Ok(0) => tr(
                    "The example LUTs are already there (existing files are never overwritten).",
                    "As LUTs de exemplo já estão lá (arquivos existentes nunca são sobrescritos).",
                )
                .to_string(),
                Ok(n) => trf!(
                    "{n} example LUTs written to GW2Graphics\\luts: identity, warm, cool, teal-orange.",
                    "{n} LUTs de exemplo gravadas em GW2Graphics\\luts: identity, warm, cool, teal-orange."
                ),
                Err(e) => trf!("Could not write the example LUTs: {e}", "Não foi possível gravar as LUTs de exemplo: {e}"),
            };
            *NOTE.lock().unwrap_or_else(|e| e.into_inner()) = msg;
            *list = Some(gr::list_luts_in(&dir));
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(tr("Writes identity / warm / cool / teal-orange .cube files (log2 shaper LUTs that keep the luminance) into GW2Graphics\\luts. Then pick one above and set the LUT strength.", "Grava os arquivos .cube identity / warm / cool / teal-orange (LUTs com shaper log2 que mantêm a luminância) em GW2Graphics\\luts. Depois escolha uma acima e ajuste a intensidade da LUT."));
        }
        let note = NOTE.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !note.is_empty() {
            ui.text_wrapped(note);
        }
        if files.is_empty() {
            help(ui, tr(
            "No .cube file in GW2Graphics\\luts yet: press Create example LUTs, or copy your own (log2 shaper, see docs/grading.md) there and press Refresh.",
            "Ainda não há arquivo .cube em GW2Graphics\\luts: aperte Criar LUTs de exemplo, ou copie as suas (shaper log2, veja docs/grading.md) para lá e aperte Atualizar lista.",
        ));
        }
    }

    fn sec_aa(&mut self, ui: &Ui) {
        help(ui, tr(
            "These keys are read from config.ini (and hot reloaded with the debug mode): smaa, smaa_threshold, sharpen_strength, aniso_level, mip_bias. Edit the file and save, or use the debug mode hot reload.",
            "Estas chaves são lidas do config.ini (e recarregadas ao vivo no modo de depuração): smaa, smaa_threshold, sharpen_strength, aniso_level, mip_bias. Edite o arquivo e salve, ou use a recarga ao vivo do modo de depuração.",
        ));
        ui.text_wrapped(trf!("Anti-aliasing: {}", "Anti-aliasing: {}", crate::aa::status_text()));
        ui.text_wrapped(trf!("Texture filtering: {}", "Filtragem de texturas: {}", crate::aniso::status_text()));
        ui.text_wrapped(format!(
            "smaa={} threshold={:.3} sharpen={:.2} aniso={} mip_bias={:.2}",
            self.cfg.smaa, self.cfg.smaa_threshold, self.cfg.sharpen_strength, self.cfg.aniso_level, self.cfg.mip_bias
        ));
    }

    fn sec_fur(&mut self, ui: &Ui) {
        help(ui, tr(
            "Fur applies only to the draws chosen below. Use 'Pick fur shaders' to choose them from the live list of skinned draws. Everything applies live; Save keeps it.",
            "A pelagem vale só para os draws escolhidos abaixo. Use 'Escolher shaders de pelagem' para escolhê-los na lista ao vivo de draws com skinning. Tudo aplica na hora; Salvar mantém.",
        ));
        let mut fs = self.cfg.fur_strength;
        if ui.slider(tr("Fur strength", "Intensidade da pelagem"), datapath::FUR_RANGE.0, datapath::FUR_RANGE.1, &mut fs) {
            self.cfg.fur_strength = datapath::set_fur(fs);
        }
        self.fur_look_controls(ui);
        self.shell_controls(ui);
        let mut dbg = self.cfg.fur_debug;
        if ui.checkbox(tr("Show selection in magenta", "Mostrar a seleção em magenta"), &mut dbg) {
            self.cfg.fur_debug = dbg;
            self.apply_fur();
        }
        let mut v = self.cfg.fur_min_vcb.min(furmask::MAX_VCB) as i32;
        if ui.input_int(tr("Min vertex cb (bytes)", "Vertex cb mínimo (bytes)"), &mut v).build() {
            self.cfg.fur_min_vcb = v.clamp(0, furmask::MAX_VCB as i32) as u32;
            self.apply_fur();
        }
        if !self.vs_active {
            self.vs_buf = self.cfg.fur_vs.clone();
        }
        if ui.input_text(tr("Vertex shaders (optional)", "Vertex shaders (opcional)"), &mut self.vs_buf).build() {
            self.cfg.fur_vs = self.vs_buf.clone();
            self.apply_fur();
        }
        self.vs_active = ui.is_item_active();
        if !self.uv_active {
            self.uv_buf = self.cfg.fur_uv.clone();
        }
        if ui.input_text(tr("Mesh uv (auto / TEXCOORD0)", "UV da malha (auto / TEXCOORD0)"), &mut self.uv_buf).build() {
            let t = self.uv_buf.trim().to_ascii_lowercase();
            self.cfg.fur_uv = if t.is_empty() { "auto".to_string() } else { t };
            furmask::set_uv_pref(&self.cfg.fur_uv);
        }
        self.uv_active = ui.is_item_active();

        ui.separator();
        ui.text(tr("Fur pixel shaders (fur_ps)", "Pixel shaders da pelagem (fur_ps)"));
        let list = furmask::parse_hashes(&self.cfg.fur_ps);
        if list.is_empty() {
            help(ui, tr(
            "Empty: nothing is fur.",
            "Vazia: nada é pelagem.",
        ));
        }
        let mut remove = None;
        for h in &list {
            ui.text(format!("{h:08x}"));
            ui.same_line();
            if ui.small_button(trf!("Remove##rm{h:08x}", "Remover##rm{h:08x}")) {
                remove = Some(*h);
            }
        }
        if let Some(h) = remove {
            self.cfg.fur_ps = furpick::list_remove(&self.cfg.fur_ps, h);
            self.apply_fur();
        }
        ui.input_text("##addhash", &mut self.add_buf).hint(tr("hash, 1 to 8 hex digits", "hash, 1 a 8 dígitos hex")).build();
        ui.same_line();
        if ui.button(tr("Add##furadd", "Adicionar##furadd")) {
            if let Some(h) = furpick::parse_hash(&self.add_buf) {
                self.cfg.fur_ps = furpick::list_add(&self.cfg.fur_ps, h);
                self.apply_fur();
                self.add_buf.clear();
            }
        }
        if ui.button(tr("Verified shaders", "Shaders verificados")) {
            self.cfg.fur_ps = furpick::CHARR_PRESET.to_string();
            self.apply_fur();
        }
        ui.same_line();
        if ui.button(tr("Clear list##furclear", "Limpar lista##furclear")) {
            self.cfg.fur_ps.clear();
            self.apply_fur();
        }
        ui.separator();
        if ui.checkbox(tr("Pick fur shaders (opens a window)", "Escolher shaders de pelagem (abre uma janela)"), &mut self.picker_open) && !self.picker_open {
            self.picker_sel = None;
        }
        ui.text_wrapped(trf!("Fur mask: {}", "Máscara de pelagem: {}", furmask::status_text()));
    }

    /// Shell fur (docs/shell-fur.md): real strands out of the silhouette. Every control applies live (shellfur::configure); Save keeps them.
    fn shell_controls(&mut self, ui: &Ui) {
        use crate::shellfur as sf;
        ui.text(tr("Shell fur (real strands; live; Save keeps them)", "Pelagem em camadas (fios de verdade; ao vivo; Salvar mantém)"));
        help(ui, tr(
            "The fur draws picked below are drawn again as 4..32 shells extruded along the surface: strands that stick out of the silhouette, dark at the roots, back-lit against a bright sky (never in the dark). Needs fur strength > 0 and the fur_ps selection above. The line status_shells lists every fur draw and whether it got shells.",
            "Os draws de pelagem escolhidos abaixo são desenhados de novo como 4..32 camadas projetadas ao longo da superfície: fios que saem da silhueta, escuros na raiz, iluminados por trás contra um céu claro (nunca no escuro). Precisa de intensidade da pelagem > 0 e da seleção fur_ps acima. A linha status_shells lista cada draw de pelagem e se ele ganhou camadas.",
        ));
        let mut c = self.cfg.shell.clone();
        let mut changed = false;
        let mut on = c.on != 0;
        if ui.checkbox(tr("Shell fur on (fur_shell)", "Pelagem em camadas ligada (fur_shell)"), &mut on) {
            c.on = on as u8;
            changed = true;
        }
        let mut n = c.shells as i32;
        if ui.slider(tr("Shells (fur_shells, 0 = off)", "Camadas (fur_shells, 0 = desligado)"), 0, sf::MAX_SHELLS as i32, &mut n) {
            c.shells = sf::clamp_shells(n as f32);
            changed = true;
        }
        changed |= ui.slider(tr("Strand length cm (shell_length)", "Comprimento do fio em cm (shell_length)"), sf::LENGTH_RANGE.0, sf::LENGTH_RANGE.1, &mut c.length);
        changed |= ui.slider(tr("Strand density (shell_density)", "Densidade dos fios (shell_density)"), sf::DENSITY_RANGE.0, sf::DENSITY_RANGE.1, &mut c.density);
        changed |= ui.slider(tr("Strand thickness (shell_thickness)", "Espessura dos fios (shell_thickness)"), sf::THICKNESS_RANGE.0, sf::THICKNESS_RANGE.1, &mut c.thickness);
        changed |= ui.slider(tr("Root darkening (shell_root_dark)", "Escurecimento da raiz (shell_root_dark)"), 0.0, 1.0, &mut c.root_dark);
        changed |= ui.slider(tr("Wind (shell_wind)", "Vento (shell_wind)"), 0.0, 1.0, &mut c.wind);
        changed |= ui.slider(tr("Back light (shell_rim)", "Luz de fundo (shell_rim)"), sf::RIM_RANGE.0, sf::RIM_RANGE.1, &mut c.rim);
        changed |= ui.slider(tr("LOD distance m (shell_lod_distance)", "Distância de LOD em m (shell_lod_distance)"), sf::LOD_RANGE.0, sf::LOD_RANGE.1, &mut c.lod);
        changed |= ui.slider(tr("Keep the face clear (shell_detail: eyes, nose, markings)", "Manter o rosto limpo (shell_detail: olhos, nariz, marcas)"), 0.0, 1.0, &mut c.detail);
        changed |= ui.slider(tr("Clumping (shell_clump)", "Agrupamento (shell_clump)"), 0.0, 1.0, &mut c.clump);
        // v3: only the texels that really are fur (not the horns, claws, nose, lips); also the old fur look and the magenta fur debug.
        changed |= ui.input_text(tr("Fur texel tests (shell_fur_mask: off / t0.a<, t0.f>0.025 ...)", "Testes de texel de pelagem (shell_fur_mask: off / t0.a<, t0.f>0.025 ...)"), &mut c.fur_mask).build();
        changed |= ui.slider(tr("Fur threshold (shell_fur_threshold)", "Limiar da pelagem (shell_fur_threshold)"), 0.0, 1.0, &mut c.fur_threshold);
        changed |= ui.slider(tr("Fur threshold softness (shell_fur_soft)", "Suavidade do limiar (shell_fur_soft)"), sf::FUR_SOFT_RANGE.0, sf::FUR_SOFT_RANGE.1, &mut c.fur_soft);
        changed |= ui.input_text(tr("No-fur uv rects (shell_uv_exclude: u0 v0 u1 v1; ...)", "Retângulos UV sem pelagem (shell_uv_exclude: u0 v0 u1 v1; ...)"), &mut c.uv_exclude).build();
        changed |= ui.slider(tr("Engine units per metre (shell_unit, 0 = auto)", "Unidades do motor por metro (shell_unit, 0 = auto)"), 0.0, 100.0, &mut c.unit);
        let mut dbg = c.debug == 1;
        if ui.checkbox(tr("Show the shells (red root .. yellow tip)", "Mostrar as camadas (raiz vermelha .. ponta amarela)"), &mut dbg) {
            c.debug = dbg as u8;
            changed = true;
        }
        let mut dbg2 = c.debug == 2;
        if ui.checkbox(tr("Show the shells per draw (shell_debug=2: a colour per fur draw, black = not fur)", "Mostrar as camadas por draw (shell_debug=2: uma cor por draw de pelagem, preto = não é pelagem)"), &mut dbg2) {
            c.debug = if dbg2 { 2 } else { 0 };
            changed = true;
        }
        changed |= ui.input_text(tr("Shell pixel shaders (shell_ps, empty = fur_ps)", "Pixel shaders das camadas (shell_ps, vazio = fur_ps)"), &mut c.ps).build();
        changed |= ui.input_text(tr("Shell vertex shaders (shell_vs, empty = any)", "Vertex shaders das camadas (shell_vs, vazio = qualquer)"), &mut c.vs).build();
        let mut v = c.min_vcb.min(furmask::MAX_VCB) as i32;
        if ui.input_int(tr("Shell min vertex cb (shell_min_vcb)", "Vertex cb mínimo das camadas (shell_min_vcb)"), &mut v).build() {
            c.min_vcb = v.clamp(0, furmask::MAX_VCB as i32) as u32;
            changed = true;
        }
        changed |= ui.input_text(tr("Index counts (shell_counts: 17694 takes, -5214 leaves out)", "Contagens de índices (shell_counts: 17694 inclui, -5214 exclui)"), &mut c.counts).build();
        changed |= ui.input_text(tr("Normal (shell_normal: face / TEXCOORDn)", "Normal (shell_normal: face / TEXCOORDn)"), &mut c.normal).build();
        if ui.button(tr("Defaults##shell", "Padrões##shell")) {
            c = sf::ShellCfg::default();
            changed = true;
        }
        if changed {
            self.cfg.shell = c.clone();
            sf::configure(&c);
        }
        ui.text_wrapped(trf!("Shells: {}", "Camadas: {}", sf::status_text()));
        ui.separator();
    }

    /// The live fur look (docs/fur-look.md): six sliders (Save writes them) and the diagnostic view (never saved). All apply live.
    fn fur_look_controls(&mut self, ui: &Ui) {
        use datapath as dp;
        ui.separator();
        ui.text(tr("Fur look (live; Save keeps the six sliders)", "Visual da pelagem (ao vivo; Salvar mantém os seis controles)"));
        let (mut amp, mut scale, mut length, mut detail, mut rim, mut sss) = (self.cfg.fur_amp, self.cfg.fur_scale, self.cfg.fur_length, self.cfg.fur_detail, self.cfg.fur_rim, self.cfg.fur_sss);
        if ui.slider(tr("Strand amplitude (fur_amp)", "Amplitude dos fios (fur_amp)"), dp::FUR_AMP_RANGE.0, dp::FUR_AMP_RANGE.1, &mut amp) {
            self.cfg.fur_amp = dp::set_fur_amp(amp);
        }
        if ui.slider(tr("Strand size (fur_scale)", "Tamanho dos fios (fur_scale)"), dp::FUR_SCALE_RANGE.0, dp::FUR_SCALE_RANGE.1, &mut scale) {
            self.cfg.fur_scale = dp::set_fur_scale(scale);
        }
        if ui.slider(tr("Strand length (fur_length)", "Comprimento dos fios (fur_length)"), dp::FUR_LENGTH_RANGE.0, dp::FUR_LENGTH_RANGE.1, &mut length) {
            self.cfg.fur_length = dp::set_fur_length(length);
        }
        if ui.slider(tr("Texture detail (fur_detail)", "Detalhe da textura (fur_detail)"), dp::FUR_DETAIL_RANGE.0, dp::FUR_DETAIL_RANGE.1, &mut detail) {
            self.cfg.fur_detail = dp::set_fur_detail(detail);
        }
        if ui.slider(tr("Outline glow (fur_rim)", "Brilho do contorno (fur_rim)"), dp::FUR_RIM_RANGE.0, dp::FUR_RIM_RANGE.1, &mut rim) {
            self.cfg.fur_rim = dp::set_fur_rim(rim);
        }
        if ui.slider(tr("Warm skin glow (fur_sss)", "Brilho quente da pele (fur_sss)"), dp::FUR_SSS_RANGE.0, dp::FUR_SSS_RANGE.1, &mut sss) {
            self.cfg.fur_sss = dp::set_fur_sss(sss);
        }
        help(ui, tr(
            "Defaults 0.32 / 1 / 5 / 1 / 0.25 / 1. Outline glow 1 is the strong outline the fur had before, 0 removes the glow and the feather of the outline. Strand size multiplies the strand cells, length is relative to the width.",
            "Padrões 0.32 / 1 / 5 / 1 / 0.25 / 1. Brilho do contorno 1 é o contorno forte que a pelagem tinha antes, 0 remove o brilho e o esfumado do contorno. O tamanho multiplica as células dos fios, o comprimento é relativo à largura.",
        ));
        let mut view = dp::fur_view() as usize;
        if ui.combo_simple_string(
            tr("Fur view (diagnostic)", "Visão da pelagem (diagnóstico)"),
            &mut view,
            &[
                tr("Off", "Desligada"),
                tr("1 Mesh uv as colour (red = no uv)", "1 UV da malha como cor (vermelho = sem UV)"),
                tr("2 Strand pattern (grey)", "2 Padrão dos fios (cinza)"),
                tr("3 Rim band (white)", "3 Faixa do contorno (branco)"),
                tr("4 Strand fade (white = full)", "4 Esmaecimento dos fios (branco = total)"),
            ],
        ) {
            self.cfg.fur_view = dp::set_fur_view(view.min(dp::FUR_VIEW_MAX as usize) as u8);
        }
        help(ui, tr(
            "Painted on the pixels of the fur mask only, also with fur strength 0 (the selection must not be empty). Never saved. Switch 'Show selection in magenta' off to see it.",
            "Pintada só nos pixels da máscara de pelagem, também com intensidade 0 (a seleção não pode estar vazia). Nunca é salva. Desligue 'Mostrar a seleção em magenta' para ver.",
        ));
        if ui.button(tr("Defaults##furlook", "Padrões##furlook")) {
            let d = dp::FurLook::default();
            self.cfg.fur_amp = d.amp;
            self.cfg.fur_scale = d.scale;
            self.cfg.fur_length = d.length;
            self.cfg.fur_detail = d.detail;
            self.cfg.fur_rim = d.rim;
            self.cfg.fur_sss = d.sss;
            dp::set_fur_look(&dp::FurLook { view: dp::fur_view(), ..d });
        }
        ui.text_wrapped(trf!("Look: {}", "Visual: {}", dp::fur_look_text()));
        ui.separator();
    }

    fn sec_rim(&mut self, ui: &Ui) {
        help(ui, tr(
            "A soft rim along the silhouette of characters (never above +25 % luminance, tinted a little towards the bright background) and a gentle warm glow of the dim-lit parts (at most +8 %). Never in the dark, in water or in fog; skipped for alpha-blended (stealth) draws. Strength 0 = off. Applies live; Save keeps it.",
            "Um contorno suave na silhueta dos personagens (nunca acima de +25 % de luminância, levemente tingido pelo fundo claro) e um brilho quente suave nas partes pouco iluminadas (no máximo +8 %). Nunca no escuro, na água ou na neblina; ignorado em draws com transparência (furtividade). Intensidade 0 = desligado. Aplica na hora; Salvar mantém.",
        ));
        let (mut rs, mut rw, mut ss) = (self.cfg.rim_strength, self.cfg.rim_width, self.cfg.subsurface_strength);
        if ui.slider(tr("Rim strength", "Intensidade do contorno"), datapath::RIM_RANGE.0, datapath::RIM_RANGE.1, &mut rs) {
            self.cfg.rim_strength = datapath::set_rim(rs);
        }
        if ui.slider(tr("Rim width (px)", "Largura do contorno (px)"), datapath::RIM_WIDTH_RANGE.0, datapath::RIM_WIDTH_RANGE.1, &mut rw) {
            self.cfg.rim_width = datapath::set_rim_width(rw);
        }
        if ui.slider(tr("Subsurface glow", "Brilho subsuperficial"), 0.0, 1.0, &mut ss) {
            self.cfg.subsurface_strength = datapath::set_subsurface(ss);
        }
        let mut dbg = self.cfg.rim_debug;
        if ui.checkbox(tr("Show selection in cyan", "Mostrar a seleção em ciano"), &mut dbg) {
            self.cfg.rim_debug = dbg;
            self.apply_rim();
        }
        let mut v = self.cfg.rim_min_vcb.min(furmask::MAX_VCB) as i32;
        if ui.input_int(tr("Rim min vertex cb (bytes)", "Vertex cb mínimo do contorno (bytes)"), &mut v).build() {
            self.cfg.rim_min_vcb = v.clamp(0, furmask::MAX_VCB as i32) as u32;
            self.apply_rim();
        }
        ui.separator();
        ui.text(tr("Rim pixel shaders (rim_ps)", "Pixel shaders do contorno (rim_ps)"));
        let list = furmask::parse_hashes(&self.cfg.rim_ps);
        if list.is_empty() {
            help(ui, tr(
            "Empty: no rim light.",
            "Vazia: sem luz de contorno.",
        ));
        }
        let mut remove = None;
        for h in &list {
            ui.text(format!("{h:08x}"));
            ui.same_line();
            if ui.small_button(trf!("Remove##rimrm{h:08x}", "Remover##rimrm{h:08x}")) {
                remove = Some(*h);
            }
        }
        if let Some(h) = remove {
            self.cfg.rim_ps = furpick::list_remove(&self.cfg.rim_ps, h);
            self.apply_rim();
        }
        ui.input_text("##addrimhash", &mut self.rim_add_buf).hint(tr("hash, 1 to 8 hex digits", "hash, 1 a 8 dígitos hex")).build();
        ui.same_line();
        if ui.button(tr("Add##rimadd", "Adicionar##rimadd")) {
            if let Some(h) = furpick::parse_hash(&self.rim_add_buf) {
                self.cfg.rim_ps = furpick::list_add(&self.cfg.rim_ps, h);
                self.apply_rim();
                self.rim_add_buf.clear();
            }
        }
        if ui.button(tr("Verified shaders##rimcharr", "Shaders verificados##rimcharr")) {
            self.cfg.rim_ps = furpick::CHARR_PRESET.to_string();
            self.apply_rim();
        }
        ui.same_line();
        if ui.button(tr("Same as fur list", "Igual à lista da pelagem")) {
            self.cfg.rim_ps = self.cfg.fur_ps.clone();
            self.apply_rim();
        }
        ui.same_line();
        if ui.button(tr("Clear list##rimclear", "Limpar lista##rimclear")) {
            self.cfg.rim_ps.clear();
            self.apply_rim();
        }
        help(ui, tr(
            "The default list is the verified charr skin. Other races and armour use other shaders: add them from the picker window of the Fur section (hashes), or use 'Same as fur list'.",
            "A lista padrão é a pele de charr verificada. Outras raças e armaduras usam outros shaders: adicione-os pela janela de escolha da seção Pelagem (hashes), ou use 'Igual à lista da pelagem'.",
        ));
        ui.text_wrapped(trf!("Rim mask: {}", "Máscara do contorno: {}", furmask::rim_status_text()));
    }

    /// The picker window: the skinned draws of the last frame, one row per group. Ticking 'fur' edits fur_ps live;
    /// hovering or clicking a row paints only that group magenta in the scene.
    fn picker_window(&mut self, ui: &Ui) {
        let [dw, dh] = ui.io().display_size;
        let mut open = true;
        let rows = furpick::rows();
        let mut hover: Option<furpick::HighlightKey> = None;
        let shown = ui
            .window(tr("Pick fur shaders###fur-picker", "Escolher shaders de pelagem###fur-picker"))
            .size([furpick::picker_width(dw, dh), 0.0], Condition::FirstUseEver)
            .size_constraints([320.0, 120.0], [dw.max(320.0), dh.max(120.0)])
            .position([60.0 + furpick::window_width(dw, dh) + 20.0, 60.0], Condition::FirstUseEver)
            .opened(&mut open)
            .build(|| {
                help(ui, tr(
            "Skinned draws of the last frame (vertex constant buffer 0 of 1 KB or more), one row per group. Hover or click a row: only that group turns magenta in the scene (head, hands, armour...). Tick 'fur' to make its pixel shader a fur shader. Save keeps the list.",
            "Draws com skinning do último quadro (vertex constant buffer 0 de 1 KB ou mais), uma linha por grupo. Passe o mouse ou clique numa linha: só aquele grupo fica magenta na cena (cabeça, mãos, armadura...). Marque 'pelo' para tornar o pixel shader dele um shader de pelagem. Salvar mantém a lista.",
        ));
                ui.text_wrapped(trf!(
                    "{} groups. {}",
                    "{} grupos. {}",
                    rows.len(),
                    if rows.is_empty() {
                        tr(
                            "Nothing yet: wait a frame, and make sure the character is on screen.",
                            "Nada ainda: espere um quadro e confira se o personagem está na tela.",
                        )
                    } else {
                        ""
                    }
                ));
                if ui.button(tr("Clear highlight", "Limpar destaque")) {
                    self.picker_sel = None;
                }
                if let Some(k) = self.picker_sel {
                    ui.same_line();
                    ui.text(trf!("selected {:08x}", "selecionado {:08x}", k.ps));
                }
                // This imgui build has no table API: the classic columns do the job.
                {
                    ui.columns(7, "fur_groups", true);
                    for c in [
                        tr("fur", "pelo"),
                        "pixel shader",
                        "vertex shader",
                        "vcb",
                        "draws",
                        tr("idx or vtx / draw", "idx ou vtx / draw"),
                        tr("texture", "textura"),
                    ] {
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
                        ui.text(r.vs.map_or(tr("unknown", "desconhecido").to_string(), |v| format!("{v:08x}")));
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
                ui.text_wrapped(trf!("Fur mask: {}", "Máscara de pelagem: {}", furmask::status_text()));
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
        help(ui, tr(
            "Saves the current frame at full resolution, with the mod's effects, into GW2Graphics\\photos: a PNG (sRGB, what an SDR screen would show), a Radiance .hdr (linear, 1.0 = paper white, values above 1.0 kept) and a .txt with every setting. Purely visual. Only one capture runs at a time; further requests wait in a queue.",
            "Salva o quadro atual em resolução total, com os efeitos do mod, em GW2Graphics\\photos: um PNG (sRGB, o que uma tela SDR mostraria), um Radiance .hdr (linear, 1.0 = branco de referência, valores acima de 1.0 mantidos) e um .txt com todas as configurações. Puramente visual. Só uma captura roda por vez; os outros pedidos esperam na fila.",
        ));
        if ui.button(tr("Capture screenshot", "Capturar imagem")) {
            photo::request(photo::PhotoKind::Single);
        }
        ui.same_line();
        if ui.button(tr("Compare with and without mod", "Comparar com e sem o mod")) {
            photo::request(photo::PhotoKind::Pair);
        }
        help(ui, tr(
            "The comparison saves two photos (-mod and -vanilla); the screen flickers to the vanilla look for about 0.3 s, then your settings are restored.",
            "A comparação salva duas fotos (-mod e -vanilla); a tela pisca para o visual original por cerca de 0.3 s e depois suas configurações voltam.",
        ));
        let mut hide = photo::hide_ui();
        if ui.checkbox(tr("Hide the UI (3D scene only)", "Esconder a interface (só a cena 3D)"), &mut hide) {
            photo::set_hide_ui(hide);
            self.cfg.photo_hide_ui = hide;
        }
        help(ui, tr(
            "Takes the 3D scene from the engine's pre-UI copy instead of the finished frame (single captures only; the comparison always includes the UI). If that copy is not seen within about 1.5 s the normal photo is taken. Save keeps this switch.",
            "Pega a cena 3D da cópia do motor anterior à interface em vez do quadro pronto (só capturas simples; a comparação sempre inclui a interface). Se essa cópia não aparecer em cerca de 1.5 s, a foto normal é tirada. Salvar mantém esta opção.",
        ));
        ui.text_wrapped(trf!("Status: {}", "Situação: {}", photo::status_text()));
        ui.text_wrapped(trf!("Hotkey: {} (config key photo_key, needs a restart)", "Tecla de atalho: {} (chave photo_key do config, precisa reiniciar)", self.cfg.photo_key));
    }

    fn sec_calib(&mut self, ui: &Ui) {
        let avail = output::calibration_available();
        help(ui, tr(
            "Test patterns for the display, drawn by the mod's own last pass in absolute nits (the overlay cannot draw them: its colours stop at paper white). Pick a pattern, move this window aside, look at the picture, set the values here, then switch the pattern off. It also ends by itself after 30 s and when this window closes.",
            "Padrões de teste para a tela, desenhados pela última passada do próprio mod em nits absolutos (o overlay não consegue desenhá-los: as cores dele param no branco de referência). Escolha um padrão, afaste esta janela, olhe a imagem, ajuste os valores aqui e desligue o padrão. Ele também termina sozinho depois de 30 s e quando esta janela fecha.",
        ));
        ui.text_wrapped(trf!("Swapchain: {}", "Swapchain: {}", output::format_status()));
        if !avail {
            ui.text_colored(ui_col([1.0, 0.7, 0.3, 1.0]), tr("Unavailable: HDR output is not active (see Rule in Debug).", "Indisponível: a saída HDR não está ativa (veja Regra em Depuração)."));
        }
        let mut cur = calib::pattern() as i32;
        let mut changed = false;
        for (label, v) in [
            (tr("Off", "Desligado"), 0),
            (tr("1 Peak", "1 Pico"), 1),
            (tr("2 Paper white", "2 Branco de referência"), 2),
            (tr("3 Black level", "3 Nível de preto"), 3),
        ] {
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
                help(ui, tr(
            "Eight blocks from 400 to 4000 nits. Raise the Peak slider until the last block you can still tell apart from the one next to it matches the peak; blocks above your display's real peak look the same. The value Windows reports for the display is shown below.",
            "Oito blocos de 400 a 4000 nits. Suba o controle de Pico até que o último bloco que você ainda distingue do vizinho corresponda ao pico; blocos acima do pico real da sua tela parecem iguais. O valor que o Windows informa para a tela aparece abaixo.",
        ));
            }
            calib::PAPER => {
                help(ui, tr(
            "Left: a white UI swatch at exactly paper white. Middle: an 18 % grey card. Right: 5 % dark grey. Set the Paper white slider so the white swatch is comfortable (not glaring) in your room, while the grey card still looks like a mid grey.",
            "Esquerda: uma amostra branca de interface exatamente no branco de referência. Meio: um cartão cinza de 18 %. Direita: cinza escuro de 5 %. Ajuste o Branco de referência para que a amostra branca fique confortável (sem ofuscar) no seu ambiente, enquanto o cartão cinza ainda parece um cinza médio.",
        ));
            }
            calib::BLACK => {
                help(ui, tr(
            "Eight dim patches from 0.005 to 1 nit on black. Tell the guidance which is the dimmest you can still see. This is read-only advice: the mod never lifts blacks, so there is no setting to change.",
            "Oito manchas escuras de 0.005 a 1 nit sobre preto. Diga ao guia qual é a mais escura que você ainda vê. É só uma orientação: o mod nunca clareia os pretos, então não há configuração para mudar.",
        ));
                let mut pick = (self.black_pick + 1) as usize;
                if ui.combo_simple_string(tr("Dimmest visible patch", "Mancha mais escura visível"), &mut pick, &[tr("none", "nenhuma"), "0.005", "0.01", "0.02", "0.05", "0.1", "0.2", "0.5", "1"]) {
                    self.black_pick = pick as i32 - 1;
                }
                ui.text_wrapped(calib::black_guidance(if self.black_pick < 0 { None } else { Some(self.black_pick as usize) }));
            }
            _ => {}
        }
        if p == calib::PEAK || p == calib::PAPER {
            let (mut pw, mut peak) = (self.cfg.paper_white_nits, self.cfg.peak_nits);
            let mut apply = false;
            if p == calib::PAPER && ui.slider(tr("Paper white (nits)", "Branco de referência (nits)"), PAPER_WHITE_RANGE.0, PAPER_WHITE_RANGE.1, &mut pw) {
                self.cfg.paper_white_nits = clamp_levels(pw, peak, 1.0).0;
            }
            apply |= p == calib::PAPER && ui.is_item_deactivated_after_edit();
            if p == calib::PEAK {
                if ui.slider(tr("Peak (nits)", "Pico (nits)"), PEAK_RANGE.0, PEAK_RANGE.1, &mut peak) {
                    self.cfg.peak_nits = clamp_levels(pw, peak, 1.0).1;
                }
                apply |= ui.is_item_deactivated_after_edit();
                match output::display_max_luminance() {
                    Some(d) => {
                        ui.text_wrapped(trf!("Display reports {d:.0} nits (block {} of 8)", "A tela informa {d:.0} nits (bloco {} de 8)", calib::peak_block_for(d) + 1));
                        if ui.button(tr("Use display value", "Usar o valor da tela")) {
                            if let Some(v) = calib::peak_from_display(Some(d), PEAK_RANGE) {
                                self.cfg.peak_nits = v;
                                apply = true;
                            }
                        }
                    }
                    None => ui.text(tr("The display does not report a peak.", "A tela não informa um pico.")),
                }
            }
            if apply && output::mode() == OutputMode::ScRgb && self.cfg.enabled {
                output::set_levels(self.cfg.paper_white_nits, self.cfg.peak_nits);
            }
            help(ui, tr(
            "Applies when you release the slider. Save keeps it.",
            "Aplica quando você solta o controle. Salvar mantém.",
        ));
        }
        ui.separator();
        let mut idx = if self.cfg.hdr_format == crate::config::HdrFormat::Hdr10 { 1 } else { 0 };
        if ui.combo_simple_string(tr("HDR format", "Formato HDR"), &mut idx, &[tr("scRGB float16 (default)", "scRGB float16 (padrão)"), tr("HDR10 (experimental)", "HDR10 (experimental)")]) {
            self.cfg.hdr_format = if idx == 1 { crate::config::HdrFormat::Hdr10 } else { crate::config::HdrFormat::Scrgb };
        }
        help(ui, tr(
            "Click Save, then restart the game. HDR10 is experimental and never used with ReShade or when Windows HDR is off; if a start with HDR10 fails to reach a stable picture it switches itself off (see docs/hdr10.md).",
            "Clique em Salvar e reinicie o jogo. O HDR10 é experimental e nunca é usado com ReShade ou com o HDR do Windows desligado; se uma inicialização com HDR10 não chegar a uma imagem estável, ele se desliga sozinho (veja docs/hdr10.md).",
        ));
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
        let title = trf!(
            "Calibration: {} - move this window aside, switch the pattern off when done",
            "Calibração: {} - afaste esta janela e desligue o padrão ao terminar",
            calib_name(p)
        );
        let sz = ui.calc_text_size(&title);
        dl.add_text([(w - sz[0]) * 0.5, h * 0.12], col, &title);
    }

    fn sec_debug(&mut self, ui: &Ui) {
        let mut level = resolve::debug_view() as usize;
        if ui.combo_simple_string(
            tr("Debug view##combo", "Visão de depuração##combo"),
            &mut level,
            &[
                tr("Off", "Desligada"),
                tr("1 Boost / occlusion", "1 Realce / oclusão"),
                tr("2 Fill", "2 Preenchimento"),
                tr("3 Material classes", "3 Classes de material"),
                tr("4 Sun", "4 Sol"),
                tr("5 Reflectivity map", "5 Mapa de refletividade"),
            ],
        ) {
            self.cfg.debug_view = resolve::set_debug_view(level.min(resolve::DEBUG_VIEW_MAX as usize) as u8);
        }
        help(ui, tr(
            "The same switch as the checkboxes below (the highest level wins) and the config key debug_view, which the debug mode hot reload applies from outside. Never saved.",
            "A mesma chave das caixas abaixo (vale o nível mais alto) e da chave debug_view do config, que a recarga ao vivo do modo de depuração aplica de fora. Nunca é salva.",
        ));
        let mut dbg = resolve::debug();
        if ui.checkbox(tr("Debug view", "Visão de depuração"), &mut dbg) {
            resolve::set_debug(dbg);
        }
        help(ui, tr(
            "Boosted pixels red, occluded pixels blue, halo sources green.",
            "Pixels realçados em vermelho, ocluídos em azul, fontes de halo em verde.",
        ));
        let mut dfill = resolve::debug_fill();
        if ui.checkbox(tr("Debug fill", "Preenchimento de depuração"), &mut dfill) {
            resolve::set_debug_fill(dfill);
        }
        help(ui, tr(
            "Left half red, top right black.",
            "Metade esquerda vermelha, canto superior direito preto.",
        ));
        let mut dmat = resolve::debug_materials();
        if ui.checkbox(tr("Material classes", "Classes de material"), &mut dmat) {
            resolve::set_debug_materials(dmat);
        }
        help(ui, tr(
            "Alpha class: low blue, mid green, high red.",
            "Classe de alfa: baixa azul, média verde, alta vermelha.",
        ));
        let mut dsun = resolve::debug_sun();
        if ui.checkbox(tr("Debug sun", "Sol de depuração"), &mut dsun) {
            resolve::set_debug_sun(dsun);
        }
        help(ui, tr(
            "Sky cyan, shaft light yellow, magenta cross, log line.",
            "Céu ciano, luz dos raios amarela, cruz magenta, linha no log.",
        ));
        if ui.button(tr("Log depth stats", "Registrar estatísticas de profundidade")) {
            depthstats::arm();
        }
        help(ui, tr(
            "Writes a 'depthstats:' block to the log at the next frame (how the sky is encoded in the depth).",
            "Grava um bloco 'depthstats:' no log no próximo quadro (como o céu está codificado na profundidade).",
        ));
        ui.separator();
        ui.text(tr("Status", "Situação"));
        let mode = output::mode();
        // Wrapped: the reason can be long (it was cut off at the window edge before).
        ui.text_wrapped(trf!("Classifier: {}", "Classificador: {}", recorder::last_verdict()));
        ui.text(trf!("Gate: {}", "Portão: {}", yes(recorder::gate_on())));
        ui.text(trf!("Output: {mode:?}", "Saída: {mode:?}"));
        match output::display_max_luminance() {
            Some(d) => ui.text_wrapped(trf!("Display peak: {d:.0} nits (reported by Windows); used peak: {:.0}", "Pico da tela: {d:.0} nits (informado pelo Windows); pico usado: {:.0}", output::peak())),
            None => ui.text(tr("Display peak: not reported", "Pico da tela: não informado")),
        }
        ui.text_wrapped(trf!("Rule: {}", "Regra: {}", output::reason()));
        ui.text_wrapped(trf!("Swapchain format: {}", "Formato da swapchain: {}", output::format_status()));
        ui.text_wrapped(trf!("Calibration: {}", "Calibração: {}", calib::status_text(output::calibration_available(), crate::log::uptime_ms())));
        ui.text_wrapped(trf!("Photo: {}", "Foto: {}", photo::status_text()));
        let fin = output::final_pass_status();
        let text = trf!("Final pass: {}", "Passada final: {}", fin.text());
        if fin.is_problem() {
            ui.text_colored(ui_col([1.0, 0.35, 0.3, 1.0]), text);
        } else {
            ui.text_wrapped(text);
        }
        ui.text(trf!(
            "Float upgrade: {} ({} targets)",
            "Ponto flutuante: {} ({} alvos)",
            if upgrade::active() { tr("active", "ativo") } else { tr("off", "desligado") },
            d3d::upgraded_count()
        ));
        ui.text(trf!("Substitution: {}", "Substituição: {}", yes(shaders::substitution_enabled())));
        ui.text_wrapped(trf!("Data path: light-adjust inputs bound {} times", "Caminho de dados: entradas de ajuste de luz ligadas {} vezes", datapath::bind_count()));
        ui.text_wrapped(trf!("AO: {}", "AO: {}", ao::status_text()));
        ui.text_wrapped(trf!("Bloom: {}", "Bloom: {}", crate::bloomfix::status_text()));
        ui.text_wrapped(trf!("Halo: {}", "Halo: {}", halo::status_text()));
        ui.text_wrapped(trf!("Exposure: {}", "Exposição: {}", crate::exposure::status_text()));
        ui.text_wrapped(trf!("Local tone: {}", "Tons locais: {}", crate::exposure::status_local_text()));
        ui.text_wrapped(trf!("Reflections: {}", "Reflexos: {}", ssr::status_text()));
        ui.text_wrapped(trf!("Rim light: {}", "Luz de contorno: {}", furmask::rim_status_text()));
        ui.text_wrapped(trf!("Sun shafts: {}", "Raios de sol: {}", shafts::status_text()));
        ui.text_wrapped(trf!("Contact shadows: {}", "Sombras de contato: {}", crate::contact::status_text()));
        ui.text_wrapped(trf!("Aerial perspective: {}", "Perspectiva atmosférica: {}", crate::aerial::status_text()));
        ui.text_wrapped(trf!("Depth stats: {}", "Estatísticas de profundidade: {}", depthstats::status_text()));
        ui.text_wrapped(trf!("Frame time: {}", "Tempo de quadro: {}", debugshot::perf_status()));
        ui.text(trf!("Copy format mismatches: {}", "Cópias com formato divergente: {}", d3d::copy_mismatches()));
        ui.text_wrapped(recorder::counters_text(d3d::hooks()));
        ui.separator();
        ui.text(tr("Debug mode (see docs/debug-mode.md)", "Modo de depuração (veja docs/debug-mode.md)"));
        if ui.button(tr("Save screenshot now", "Salvar imagem agora")) {
            debugshot::request_single();
        }
        ui.same_line();
        if ui.button(tr("Save screenshot pair now", "Salvar par de imagens agora")) {
            debugshot::request_pair();
        }
        let mut secs = debugshot::capture_seconds() as i32;
        if ui.slider(tr("Capture every (s, 0 = off)", "Capturar a cada (s, 0 = desligado)"), 0, 120, &mut secs) {
            debugshot::set_capture_seconds(secs.max(0) as u32);
            self.cfg.debug_capture_seconds = debugshot::capture_seconds();
        }
        let mut cmp = debugshot::compare();
        if ui.checkbox(tr("Capture mod + vanilla pair", "Capturar par mod + vanilla"), &mut cmp) {
            debugshot::set_compare(cmp);
            self.cfg.debug_compare = cmp;
        }
        help(ui, tr(
            "The screen flickers about 0.3 s per pair.",
            "A tela pisca cerca de 0.3 s a cada par.",
        ));
        let mut hot = debugshot::hot_reload();
        if ui.checkbox(tr("Hot reload config.ini", "Recarregar config.ini ao vivo"), &mut hot) {
            debugshot::set_hot_reload(hot);
            self.cfg.debug_hot_reload = hot;
        }
        ui.text_wrapped(trf!("Debug mode: {}", "Modo de depuração: {}", debugshot::status()));
        if ui.button(tr("Dump frame", "Despejar quadro")) {
            recorder::request_dump();
            // Also arms the per-draw capture of the next frame (draws-<unix>.log).
        }
        ui.text_wrapped(trf!("Draw capture: {}", "Captura de draws: {}", crate::drawdiag::status_text()));
        let d = recorder::dump_status();
        if !d.is_empty() {
            ui.text_wrapped(&d);
        }
    }

    fn footer(&mut self, ui: &Ui) {
        if ui.button(tr("Save###save", "Salvar###save")) {
            self.note = match self.cfg.save(&self.path) {
                Ok(()) => {
                    log!("overlay: config saved");
                    tr("Saved config.ini", "config.ini salvo").to_string()
                }
                Err(e) => {
                    log!("overlay: could not save {}: {e}", self.path.display());
                    trf!("Could not save: {e}", "Não foi possível salvar: {e}")
                }
            };
        }
        if !self.note.is_empty() {
            ui.same_line();
            ui.text_wrapped(&self.note);
        }
    }
}

impl Overlay {
    /// Work that must happen every frame whether or not the panel is open: resolve, output, calibration, debug shots.
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
        // Debug mode: the back buffer copy is issued here, before the overlay is drawn (so the shot has no overlay).
        debugshot::on_frame();
        if let Some((old, new)) = debugshot::take_reload() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (merged, d) = debugshot::merge(&self.cfg, &old, &new);
                debugshot::apply_live(&d.live, &merged);
                debugshot::log_reload(&d);
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
    debugshot::start(path.clone(), cfg.clone());
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
