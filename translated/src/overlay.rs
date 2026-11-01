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
//! Text shown here follows the selected language ([`crate::ui_text`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hudhook::hooks::dx11::ImguiDx11Hooks;
use hudhook::imgui::{Condition, Context, Io, Ui, WindowFlags};
use hudhook::windows::Win32::Foundation::HINSTANCE;
use hudhook::{Hudhook, ImguiRenderLoop, MessageFilter, RenderContext};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::{Contribute, Device};
use crate::runtime::{self, Flag};
use crate::ui_text::{T, language_name, text};
use crate::{gpu, live, log, refresh};

/// The first-time setup waits for the game to finish its first loading screen.
const INVITE_DELAY: Duration = Duration::from_secs(20);
/// Steps of the first-time setup.
const WIZARD_STEPS: usize = 5;
/// How often the graphics card list (free memory) is read again.
const GPU_REFRESH: Duration = Duration::from_secs(5);

static VISIBLE: AtomicBool = AtomicBool::new(false);
static INVITING: AtomicBool = AtomicBool::new(false);

struct Overlay {
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
        text(&self.language, t)
    }

    fn poll_key(&mut self) {
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
    fn wizard(&mut self, ui: &Ui) {
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
                        self.language_picker(ui);
                    }
                    1 => {
                        ui.text(self.t(T::WizTranslateTitle));
                        ui.separator();
                        self.flag(ui, Flag::Translate, T::Translate);
                        self.flag(ui, Flag::LiveTranslate, T::LiveTranslate);
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
            ui.text_disabled(self.t(T::LanguageRestart));
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
            ui.text_disabled(none);
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
            None => ui.text_disabled(format!("{} {}", self.t(T::Resources), self.t(T::TranslatorOff))),
        }
        if runtime::device() != Device::Cpu && in_use.starts_with("cpu") {
            ui.text_disabled(self.t(T::GpuComing));
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
        ui.text_disabled(self.t(T::BubbleWaitHint));
    }

    fn main_window(&mut self, ui: &Ui) {
        let mut open = true;
        ui.window(format!("{}  v{}###gw2translated", self.t(T::Title), crate::VERSION))
            .size([460.0, 0.0], Condition::FirstUseEver)
            .opened(&mut open)
            .flags(WindowFlags::NO_COLLAPSE | WindowFlags::ALWAYS_AUTO_RESIZE)
            .build(|| {
                self.language_picker(ui);

                ui.separator();
                ui.text(self.t(T::Options));
                self.flag(ui, Flag::Translate, T::Translate);
                self.flag(ui, Flag::LiveTranslate, T::LiveTranslate);
                let mut collect = runtime::get(Flag::Autocollect) && runtime::get(Flag::SeedDecode);
                if ui.checkbox(self.t(T::Collect), &mut collect) {
                    runtime::set(Flag::Autocollect, collect);
                    runtime::set(Flag::SeedDecode, collect);
                }
                let mut contribute = runtime::contribute() == Contribute::Yes;
                if ui.checkbox(self.t(T::Contribute), &mut contribute) {
                    runtime::set_contribute(contribute);
                }
                ui.text_disabled(self.t(T::ContributeHint));

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
                    None => ui.text_disabled(self.t(T::StatusLiveOff)),
                }
                ui.text_disabled(self.t(T::CloseHint));
            });
        if !open {
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }

    fn flag(&self, ui: &Ui, flag: Flag, label: T) {
        let mut on = runtime::get(flag);
        if ui.checkbox(self.t(label), &mut on) {
            runtime::set(flag, on);
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

pub fn install(module: usize, language: &str, languages_dir: PathBuf, key: u32) {
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
    let hooks = Hudhook::builder()
        .with::<ImguiDx11Hooks>(overlay)
        .with_hmodule(HINSTANCE(module as *mut _))
        .build();
    match hooks.apply() {
        Ok(()) => log!("overlay: installed (key {key:#x})"),
        Err(e) => log!("overlay: could not install: {e:?}"),
    }
}
