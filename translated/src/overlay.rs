//! In-game overlay (Dear ImGui on the game's DirectX 11 swap chain, via `hudhook`).
//!
//! - The overlay key (default Insert) opens and closes it; while it is open,
//!   keyboard and mouse go to the overlay, not to the game.
//! - Language selector (saved to `config.ini`, used after a restart).
//! - Options that apply immediately (see [`crate::runtime`]).
//! - "Refresh texts now" and a status line.
//! - A first-time setup (onboarding) shown until it is completed: language,
//!   what to translate, where the local translator runs (GPU or CPU), speech
//!   bubbles and the invitation to contribute.
//!
//! Text shown here follows the overlay language ([`gw2enhanced_core::i18n`], [`crate::ui_text`]), which is not the
//! language the game is translated into.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hudhook::imgui::{Condition, Context, Io, Ui, WindowFlags};
use hudhook::{ImguiRenderLoop, MessageFilter, RenderContext};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::{Contribute, Device};
use crate::runtime::{self, Flag};
use crate::ui_text::{T, language_name, text};
use gw2enhanced_core::i18n::{self, Lang};
use crate::{gpu, live, log, refresh};

/// The first-time setup waits for the game to finish its first loading screen.
const INVITE_DELAY: Duration = Duration::from_secs(20);
/// Steps of the first-time setup.
const WIZARD_STEPS: usize = 5;
/// How often the graphics card list (free memory) is read again.
const GPU_REFRESH: Duration = Duration::from_secs(5);

static VISIBLE: AtomicBool = AtomicBool::new(false);
static INVITING: AtomicBool = AtomicBool::new(false);

/// The window or the invitation is on screen (the host then draws the cursor and passes input through).
pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::Relaxed) || INVITING.load(Ordering::Relaxed)
}

/// Opens or closes the window (the host uses it when translation is the only feature built in).
pub fn set_visible(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
}

/// Set when the panel asks to be closed (the unified overlay owns the window).
pub static HIDE_HOST: AtomicBool = AtomicBool::new(false);

pub struct Overlay {
    key: i32,
    key_down: bool,
    started: Instant,
    /// Language in use (the one loaded at startup).
    language: String,
    languages: Vec<String>,
    selected: usize,
    language_changed: bool,
    /// Current step of the first-time setup.
    step: usize,
    /// Run the first-time setup again (from the main window).
    wizard_again: bool,
    gpus: Vec<gpu::Gpu>,
    gpus_read: Option<Instant>,
}

impl Overlay {
    fn t(&self, t: T) -> &'static str {
        text(i18n::current(), t)
    }

    pub fn poll_key(&mut self) {
        let down = unsafe { GetAsyncKeyState(self.key) } as u16 & 0x8000 != 0;
        if down && !self.key_down && game_has_focus() {
            VISIBLE.fetch_xor(true, Ordering::Relaxed);
        }
        self.key_down = down;
    }

    fn gpus(&mut self) -> &[gpu::Gpu] {
        if self.gpus_read.is_none_or(|t| t.elapsed() >= GPU_REFRESH) {
            self.gpus = gpu::list();
            self.gpus_read = Some(Instant::now());
        }
        &self.gpus
    }

    fn finish_wizard(&mut self) {
        runtime::set_onboarded();
        self.wizard_again = false;
        self.step = 0;
    }

    /// First-time setup, centered, until completed or skipped.
    pub fn wizard(&mut self, ui: &Ui) {
        let show = (self.wizard_again || !runtime::onboarded()) && self.started.elapsed() >= INVITE_DELAY;
        INVITING.store(show, Ordering::Relaxed);
        if !show {
            return;
        }
        let [w, h] = ui.io().display_size;
        let title = format!(
            "{}  ({} {}/{})###gw2translated-setup",
            self.t(T::WizTitle),
            self.t(T::WizStep),
            self.step + 1,
            WIZARD_STEPS
        );
        ui.window(title)
            .position([w / 2.0, h / 2.0], Condition::Always)
            .position_pivot([0.5, 0.5])
            .size([620.0 * ui.io().font_global_scale, 0.0], Condition::Always)
            .flags(WindowFlags::NO_COLLAPSE | WindowFlags::NO_RESIZE | WindowFlags::NO_MOVE)
            .build(|| {
                match self.step {
                    0 => {
                        ui.text_wrapped(self.t(T::WizIntro));
                        ui.spacing();
                        overlay_language_picker(ui);
                        ui.spacing();
                        self.language_picker(ui);
                    }
                    1 => {
                        ui.text(self.t(T::WizTranslateTitle));
                        ui.separator();
                        self.flag(ui, Flag::Translate, T::Translate);
                        self.flag(ui, Flag::LiveTranslate, T::LiveTranslate);
                        self.flag(ui, Flag::WebTranslate, T::WebTranslate);
                    }
                    2 => {
                        ui.text(self.t(T::WizDeviceTitle));
                        ui.separator();
                        ui.text_wrapped(self.t(T::WizDeviceHint));
                        ui.spacing();
                        self.device_picker(ui);
                    }
                    3 => {
                        ui.text(self.t(T::WizBubbleTitle));
                        ui.separator();
                        self.bubble_wait(ui);
                    }
                    _ => {
                        ui.text(self.t(T::PopupTitle));
                        ui.separator();
                        ui.text_wrapped(self.t(T::PopupBody));
                        ui.spacing();
                        if ui.button(self.t(T::PopupYes)) {
                            runtime::set_contribute(true);
                            self.finish_wizard();
                        }
                        ui.same_line();
                        if ui.button(self.t(T::PopupNo)) {
                            runtime::set_contribute(false);
                            self.finish_wizard();
                        }
                    }
                }
                ui.spacing();
                ui.separator();
                if self.step > 0 && ui.button(self.t(T::Back)) {
                    self.step -= 1;
                }
                if self.step + 1 < WIZARD_STEPS {
                    if self.step > 0 {
                        ui.same_line();
                    }
                    if ui.button(self.t(T::Next)) {
                        self.step += 1;
                    }
                    ui.same_line();
                    if ui.button(self.t(T::Skip)) {
                        self.finish_wizard();
                    }
                }
            });
    }

    fn language_picker(&mut self, ui: &Ui) {
        ui.text(self.t(T::Language));
        let names: Vec<String> = self.languages.iter().map(|c| language_name(c)).collect();
        if ui.combo_simple_string("##language", &mut self.selected, &names)
            && let Some(code) = self.languages.get(self.selected)
        {
            runtime::save("language", code);
            self.language_changed = *code != self.language;
        }
        if self.language_changed {
            hint(ui, self.t(T::LanguageRestart));
        }
    }

    /// Device choice for the local translator, with the cards found.
    fn device_picker(&mut self, ui: &Ui) {
        let labels = [T::DeviceAuto, T::DeviceCuda, T::DeviceDirectMl, T::DeviceCpu];
        let mut current = runtime::device();
        for (d, label) in Device::ALL.into_iter().zip(labels) {
            if ui.radio_button(self.t(label), &mut current, d) {
                runtime::set_device(d);
            }
        }
        ui.spacing();
        let (detected, none, free, short) = (
            self.t(T::GpuDetected),
            self.t(T::GpuNone),
            self.t(T::GpuFree),
            self.t(T::GpuNotEnough),
        );
        let gpus = self.gpus().to_vec();
        if gpus.iter().any(gpu::Gpu::has_room) {
            ui.text(detected);
            for g in &gpus {
                let free_mb = g.free_mb.map_or("?".into(), |f| f.to_string());
                let note = if g.has_room() { "" } else { short };
                ui.bullet_text(format!("{} ({}): {free_mb} {free} {note}", g.name, g.vendor_name()));
            }
        } else {
            hint(ui, none);
        }
        let in_use = live::active_device();
        if !in_use.is_empty() {
            ui.text(format!("{} {in_use}", self.t(T::DeviceInUse)));
        }
        match crate::procstats::usage() {
            Some(u) => {
                let mut line = format!("{} RAM {} MB · CPU {:.0}%", self.t(T::Resources), u.ram_mb, u.cpu_pct);
                if let (Some(vram), Some(gpu)) = (u.vram_mb, u.gpu_pct) {
                    line.push_str(&format!(" · VRAM {vram} MB · GPU {gpu:.0}%"));
                }
                ui.text(line);
            }
            None => hint(ui, format!("{} {}", self.t(T::Resources), self.t(T::TranslatorOff))),
        }
        if runtime::device() != Device::Cpu && in_use.starts_with("cpu") {
            hint(ui, self.t(T::GpuComing));
        }
        let mut threads = runtime::threads() as i32;
        ui.text_wrapped(self.t(T::Threads));
        ui.set_next_item_width(160.0 * ui.io().font_global_scale);
        if ui.input_int("##threads", &mut threads).build() {
            runtime::set_threads(threads.clamp(0, 64) as u32);
        }
    }

    fn bubble_wait(&self, ui: &Ui) {
        let mut wait = runtime::bubble_wait();
        ui.text_wrapped(self.t(T::BubbleWait));
        ui.set_next_item_width(240.0 * ui.io().font_global_scale);
        if ui.slider("##bubble_wait", 0, 30, &mut wait) {
            runtime::set_bubble_wait(wait);
        }
        hint(ui, self.t(T::BubbleWaitHint));
    }

    /// The panel's content, without a window around it (the unified overlay puts it in a tab).
    pub fn body(&mut self, ui: &Ui) {
        self.flag(ui, Flag::Translate, T::Translate);
        hint(ui, i18n::tr("Off: the game's original text, live.", "Desligada: o texto original do jogo, na hora."));
        ui.separator();
        self.language_picker(ui);

        ui.separator();
        ui.text(self.t(T::Options));
        self.flag(ui, Flag::LiveTranslate, T::LiveTranslate);
        self.flag(ui, Flag::WebTranslate, T::WebTranslate);
        let mut collect = runtime::get(Flag::Autocollect) && runtime::get(Flag::SeedDecode);
        if ui.checkbox(self.t(T::Collect), &mut collect) {
            runtime::set(Flag::Autocollect, collect);
            runtime::set(Flag::SeedDecode, collect);
        }
        let mut contribute = runtime::contribute() == Contribute::Yes;
        if ui.checkbox(self.t(T::Contribute), &mut contribute) {
            runtime::set_contribute(contribute);
        }
        hint(ui, self.t(T::ContributeHint));
        self.dictionary_updates(ui);

        ui.separator();
        if ui.collapsing_header(self.t(T::Performance), hudhook::imgui::TreeNodeFlags::empty()) {
            self.device_picker(ui);
            ui.spacing();
            self.bubble_wait(ui);
        }

        ui.separator();
        if ui.button(self.t(T::OnboardAgain)) {
            self.wizard_again = true;
            self.step = 0;
            VISIBLE.store(false, Ordering::Relaxed);
        HIDE_HOST.store(true, Ordering::Relaxed);
        }
        if ui.button(self.t(T::RefreshNow)) {
            refresh::request_now();
        }

        ui.separator();
        ui.text(self.t(T::Status));
        let (seen, translated) = crate::hooks::stats();
        ui.text(format!("{seen} / {translated}  {}", self.t(T::StatusTexts)));
        match live::stats() {
            Some((screen, ahead, queued)) => {
                ui.text(format!("{screen} / {ahead} / {queued}  {}", self.t(T::StatusLive)))
            }
            None => hint(ui, self.t(T::StatusLiveOff)),
        }
        hint(ui, self.t(T::CloseHint));
    }

    fn main_window(&mut self, ui: &Ui) {
        let mut open = true;
        ui.window(format!("{}  {}###gw2translated", self.t(T::Title), gw2enhanced_core::release::label()))
            .size([460.0, 0.0], Condition::FirstUseEver)
            .opened(&mut open)
            .flags(WindowFlags::NO_COLLAPSE | WindowFlags::ALWAYS_AUTO_RESIZE)
            .build(|| self.body(ui));
        if !open {
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }

    /// Automatic dictionary downloads (`release.rs`) and the version in use.
    fn dictionary_updates(&self, ui: &Ui) {
        self.flag(ui, Flag::UpdateDictionary, T::UpdateDictionary);
        match crate::release::in_use() {
            Some(v) => ui.text_disabled(format!("{} {v}", self.t(T::DictionaryVersion))),
            None => ui.text_disabled(self.t(T::DictionaryShipped)),
        }
    }

    fn flag(&self, ui: &Ui, flag: Flag, label: T) {
        let mut on = runtime::get(flag);
        if ui.checkbox(self.t(label), &mut on) {
            runtime::set(flag, on);
            // The text already on screen follows too (back to the game's own text when turned off).
            if flag == Flag::Translate {
                refresh::request_now();
            }
        }
    }
}

impl ImguiRenderLoop for Overlay {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        // The settings file would land in the game folder.
        ctx.set_ini_filename(None);
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        let interactive = VISIBLE.load(Ordering::Relaxed) || INVITING.load(Ordering::Relaxed);
        let io = ctx.io_mut();
        // Readable on 1080p and above; the default font is 13 px.
        io.font_global_scale = (io.display_size[1] / 1080.0).clamp(1.0, 2.0);
        io.mouse_draw_cursor = interactive;
    }

    fn render(&mut self, ui: &mut Ui) {
        self.poll_key();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.wizard(ui);
            if VISIBLE.load(Ordering::Relaxed) {
                self.main_window(ui);
            }
        }));
        if result.is_err() {
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }

    fn message_filter(&self, _io: &Io) -> MessageFilter {
        if VISIBLE.load(Ordering::Relaxed) || INVITING.load(Ordering::Relaxed) {
            MessageFilter::InputAll
        } else {
            MessageFilter::empty()
        }
    }
}

/// The overlay language (the same setting as the selector above the main window's tabs), for the first-time setup,
/// which is a window of its own.
fn overlay_language_picker(ui: &Ui) {
    let cur = i18n::current();
    ui.text(i18n::tr("Overlay language", "Idioma do overlay"));
    if let Some(_c) = ui.begin_combo("##wizard-overlay-language", cur.name()) {
        for l in Lang::ALL {
            if ui.selectable_config(l.name()).selected(l == cur).build()
                && l != cur
                && let Err(e) = i18n::choose(l)
            {
                log!("overlay: could not save ui_language: {e}");
            }
        }
    }
}

/// A dimmed note, wrapped at the window edge (the host window has a fixed width, so long lines must not be cut).
fn hint(ui: &Ui, text: impl AsRef<str>) {
    let c = ui.style_color(hudhook::imgui::StyleColor::TextDisabled);
    let _t = ui.push_style_color(hudhook::imgui::StyleColor::Text, c);
    ui.text_wrapped(text);
}

fn game_has_focus() -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == unsafe { GetCurrentProcessId() }
}

/// Language folders available in `languages\`.
fn available_languages(languages_dir: &PathBuf) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(languages_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("dictionary.jsonl").is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|c| crate::paths::valid_language(c))
        .collect();
    out.sort();
    out
}

/// Panel waiting for the unified overlay to pick it up (when the DLL is not standalone).
pub static PANEL: std::sync::Mutex<Option<Overlay>> = std::sync::Mutex::new(None);

pub fn install(_module: usize, language: &str, languages_dir: PathBuf, key: u32) {
    let mut languages = available_languages(&languages_dir);
    if !languages.iter().any(|l| l == language) {
        languages.insert(0, language.to_owned());
    }
    let selected = languages.iter().position(|l| l == language).unwrap_or(0);
    let overlay = Overlay {
        key: key as i32,
        key_down: false,
        started: Instant::now(),
        language: language.to_owned(),
        languages,
        selected,
        language_changed: false,
        step: 0,
        wizard_again: false,
        gpus: Vec::new(),
        gpus_read: None,
    };
    *PANEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(overlay);
    log!("overlay: panel ready for the unified overlay");
}
