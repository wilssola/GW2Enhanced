//! In-game overlay (Dear ImGui via `hudhook`, same stack as GW2 Translated).
//!
//! - **Shift+Home** (configurable, `overlay_key`) opens/closes it; GW2 Translated uses Insert.
//! - "Remaster ativo" toggles serving the HD pack. It applies to textures the
//!   game loads from then on; armor composites keep their 2048 layout either way
//!   (when off, composite pieces get a plain 2x of the original, which looks the
//!   same as the original), so toggling never breaks the composite.
//! - Choices are saved to `GW2Remastered\config.ini` (same style as GW2 Translated).
//! - Live counters: textures served from the pack, plain 2x, refused resizes.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use hudhook::imgui::{Condition, Context, Io, Ui};
use hudhook::{ImguiRenderLoop, MessageFilter, RenderContext};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::log;
use gw2enhanced_core::i18n::tr;
use gw2enhanced_core::trf;

const VK_HOME: i32 = 0x24;

static VISIBLE: AtomicBool = AtomicBool::new(false);
/// Serve the HD pack. Read on every texture load (see `diag::block_cb`).
pub static ENABLED: AtomicBool = AtomicBool::new(true);
pub static N_PACK: AtomicU32 = AtomicU32::new(0);
pub static N_PLAIN2X: AtomicU32 = AtomicU32::new(0);
pub static N_REFUSED: AtomicU32 = AtomicU32::new(0);
pub static COMPOSITE_2X: AtomicBool = AtomicBool::new(false);
pub static PACK_SIZE: AtomicU32 = AtomicU32::new(0);
static SETTINGS: OnceLock<PathBuf> = OnceLock::new();

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The panel is open (memory figures are only gathered then).
pub fn set_visible(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
}

pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}

/// Pieces already loading when HD is toggled must not mix with the new mode: an HD color
/// piece over an original mask is the engine's fatal "Composite mask smaller than source
/// RGB". So pieces switch this long after the click, and the rebuild runs after that.
pub const PIECE_SWITCH: std::time::Duration = std::time::Duration::from_secs(3);
static TOGGLED_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// HD/upscaled mode for composite pieces (follows `enabled` after `PIECE_SWITCH`).
pub fn pieces_on() -> bool {
    let on = enabled();
    let recent = TOGGLED_AT.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() < PIECE_SWITCH);
    if recent { !on } else { on }
}


/// Overlay key (virtual code), from `overlay_key` in config.ini.
static KEY: AtomicU32 = AtomicU32::new(VK_HOME as u32);
/// Modifiers that must be held with KEY (MOD_* bits). Default: Shift+Home.
static MODS: AtomicU32 = AtomicU32::new(MOD_SHIFT);
/// Folder with the HD pack (`pack_dir` in config.ini); empty = `GW2Remastered\pack`.
static PACK_DIR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// The overlay is waiting for the new hotkey.
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// No HD pack was found at start: the game keeps its original textures (set by `diag::start`).
pub static NO_PACK: AtomicBool = AtomicBool::new(false);
/// The user finished (or skipped) the first-run folder selection (`setup_done` in config.ini).
static SETUP_DONE: AtomicBool = AtomicBool::new(false);
/// A new folder was saved; the pack is read at game start, so it applies after a restart.
static RESTART_NEEDED: AtomicBool = AtomicBool::new(false);
/// A folder dialog is open (it runs on its own thread, so the game keeps rendering).
static PICKING: AtomicBool = AtomicBool::new(false);
/// Folder chosen in the dialog, waiting to be shown in the text box.
static PICKED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Pack folder: `pack_dir` from config.ini, else `<data>\pack`.
pub fn pack_dir(data: &std::path::Path) -> PathBuf {
    let d = PACK_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if d.is_empty() { data.join("pack") } else { PathBuf::from(d) }
}

/// A folder holds an HD pack when it has the archive index or the loose-file index.
pub fn has_pack(dir: &std::path::Path) -> bool {
    dir.join("pack.idx").is_file() || dir.join("index.jsonl").is_file()
}

/// The folder the user picked, or its `pack` subfolder when that is where the pack is.
fn resolve_pack(dir: &str) -> Option<PathBuf> {
    let d = PathBuf::from(dir.trim().trim_matches('"'));
    [d.clone(), d.join("pack")].into_iter().find(|p| has_pack(p))
}

/// Folder dialog (Windows shell). Blocks, so it must run off the render thread.
fn pick_folder() -> Option<String> {
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::{
        BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
    };
    unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let title: Vec<u16> = tr(
            "Select the folder with the GW2 Remastered HD textures",
            "Escolha a pasta com as texturas HD do GW2 Remastered",
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        bi.hwndOwner = GetForegroundWindow();
        bi.lpszTitle = title.as_ptr();
        bi.ulFlags = BIF_NEWDIALOGSTYLE | BIF_RETURNONLYFSDIRS;
        let pidl = SHBrowseForFolderW(&bi);
        let mut out = None;
        if !pidl.is_null() {
            let mut buf = [0u16; 520];
            if SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0 {
                let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                out = Some(String::from_utf16_lossy(&buf[..n]));
            }
            CoTaskMemFree(pidl as _);
        }
        if hr >= 0 {
            CoUninitialize();
        }
        out
    }
}

fn start_picker() {
    if PICKING.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new().name("gw2remastered-picker".into()).spawn(|| {
        let r = std::panic::catch_unwind(pick_folder).ok().flatten();
        *PICKED.lock().unwrap_or_else(|e| e.into_inner()) = r;
        PICKING.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        PICKING.store(false, Ordering::Relaxed);
    }
}

/// `key=value` lines of an ini file (comments `#`/`;`, blanks ignored).
fn ini_get(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim().to_string())
}

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 2;
const MOD_ALT: u32 = 4;

/// One key: a name (`Home`, `Insert`, `F1`..`F12`, ...) or a virtual code (`0x24`, `36`).
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

/// `Shift+Home`, `Ctrl+Alt+F5`, `0x24`... -> (virtual key, modifier bits).
fn parse_key(v: &str) -> Option<(u32, u32)> {
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

fn key_name(vk: u32) -> String {
    match vk {
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x13 => "Pause".into(),
        0x91 => "ScrollLock".into(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        _ => format!("{vk:#04x}"),
    }
}

/// Hotkey as written in config.ini, e.g. `Shift+Home`.
fn hotkey_text() -> String {
    let m = MODS.load(Ordering::Relaxed);
    let mut s = String::new();
    if m & MOD_CTRL != 0 {
        s.push_str("Ctrl+");
    }
    if m & MOD_ALT != 0 {
        s.push_str("Alt+");
    }
    if m & MOD_SHIFT != 0 {
        s.push_str("Shift+");
    }
    s + &key_name(KEY.load(Ordering::Relaxed))
}

/// Reads `GW2Remastered\config.ini` (same style as GW2 Translated). A missing
/// file is created with defaults; an old `settings.json` is migrated once.
pub fn load_settings(path: PathBuf) {
    let old_json = path.with_file_name("settings.json");
    if !path.exists() {
        if let Ok(j) = std::fs::read_to_string(&old_json) {
            let c = j.replace(' ', "");
            ENABLED.store(!c.contains("\"enabled\":false"), Ordering::Relaxed);
            let _ = std::fs::remove_file(&old_json);
            log!("config: migrated settings.json");
        }
    } else if let Ok(t) = std::fs::read_to_string(&path) {
        if let Some(v) = ini_get(&t, "remaster") {
            ENABLED.store(!v.eq_ignore_ascii_case("false"), Ordering::Relaxed);
        }
        // composite scale is not a user option: 4x for the local player, 2x for others,
        // or 4x for every player with composite_4x_all
        if let Some(v) = ini_get(&t, "composite_4x_all") {
            crate::composite::ALL_4X.store(v.eq_ignore_ascii_case("true"), Ordering::Relaxed);
        }
        if ini_get(&t, "debug_tint").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            crate::diag::DEBUG_TINT.store(true, Ordering::Relaxed);
            log!("config: DEBUG TINT ON (replaced textures shown in flat colours)");
        }
        if let Some(v) = ini_get(&t, "live_reload_npc") {
            crate::reload::LIVE_NPC.store(!v.eq_ignore_ascii_case("false"), Ordering::Relaxed);
        }
        if let Some(v) = ini_get(&t, "live_reload_models") {
            crate::reload::LIVE_MODELS.store(!v.eq_ignore_ascii_case("false"), Ordering::Relaxed);
        }
        if let Some(v) = ini_get(&t, "composite_4x") {
            crate::composite::ALLOW_4X.store(v.eq_ignore_ascii_case("true"), Ordering::Relaxed);
        }
        if let Some(v) = ini_get(&t, "terrain_hd") {
            crate::diag::TERRAIN_HD.store(!v.eq_ignore_ascii_case("false"), Ordering::Relaxed);
        }
        if let Some(d) = ini_get(&t, "pack_dir") {
            *PACK_DIR.lock().unwrap_or_else(|e| e.into_inner()) = d;
        }
        if let Some(v) = ini_get(&t, "setup_done") {
            SETUP_DONE.store(v.eq_ignore_ascii_case("true"), Ordering::Relaxed);
        }
        match ini_get(&t, "overlay_key").as_deref() {
            // old default (plain Home, written as 0x24): move to the new default Shift+Home
            Some("0x24") | Some("0x24 ") => {}
            Some(v) => {
                if let Some((k, m)) = parse_key(v) {
                    KEY.store(k, Ordering::Relaxed);
                    MODS.store(m, Ordering::Relaxed);
                }
            }
            None => {}
        }
    }
    let _ = SETTINGS.set(path);
    // also rewrites an old file in the current format (e.g. overlay_key=0x24 -> Shift+Home)
    let stale = SETTINGS
        .get()
        .is_some_and(|p| {
            std::fs::read_to_string(p).map_or(true, |t| {
                ini_get(&t, "overlay_key").as_deref() != Some(hotkey_text().as_str()) || ini_get(&t, "composite_scale").is_some() || ini_get(&t, "composite_4x_all").is_none()
                    || ini_get(&t, "setup_done").is_none()
            })
        });
    if stale {
        save_settings();
    }
    log!(
        "config: remaster {}, 4x skin/face/armor for {}, overlay key {}",
        if enabled() { "ON" } else { "OFF" },
        if crate::composite::ALL_4X.load(Ordering::Relaxed) { "every player" } else { "the local player" },
        hotkey_text()
    );
}

fn save_settings() {
    if let Some(p) = SETTINGS.get() {
        let body = format!(
            concat!(
                "# GW2 Remastered — configuration\n",
                "\n",
                "# Serve the HD textures (also toggled in the overlay)\n",
                "remaster={}\n",
                "\n",
                "# 4x skin, face and armor for every player on screen (true) or only for\n",
                "# your character (false, saves memory). Applies to characters loaded after the change.\n",
                "composite_4x_all={}\n",
                "\n",
                "# Overlay hotkey: modifiers (Shift, Ctrl, Alt) + key (Home, End, Insert, F1..F24, A..Z, 0..9)\n",
                "# or virtual code (0x24). Can also be changed in the overlay. GW2 Translated uses Insert.\n",
                "overlay_key={}\n",
                "\n",
                "# Folder with the HD pack (empty = GW2Remastered\\pack next to the game). Without a pack\n",
                "# there the game shows its original textures. Choose it in the overlay (read at game start).\n",
                "pack_dir={}\n",
                "\n",
                "# First-run folder selection finished or skipped (the overlay stops asking)\n",
                "setup_done={}\n",
                "\n",
                "# Live reload when toggling HD textures: NPCs and model textures (scenery, weapons).\n",
                "# Set to false if toggling crashes the game.\n",
                "live_reload_npc={}\n",
                "live_reload_models={}\n",
                "\n",
                "# 4x characters at all (false = 2x for everyone)\n",
                "composite_4x={}\n",
                "\n",
                "# Serve the upscaled terrain tiles (read at start)\n",
                "terrain_hd={}\n",
            ),
            enabled(),
            crate::composite::ALL_4X.load(Ordering::Relaxed),
            hotkey_text(),
            PACK_DIR.lock().unwrap_or_else(|e| e.into_inner()),
            SETUP_DONE.load(Ordering::Relaxed),
            crate::reload::LIVE_NPC.load(Ordering::Relaxed),
            crate::reload::LIVE_MODELS.load(Ordering::Relaxed),
            crate::composite::ALLOW_4X.load(Ordering::Relaxed),
            crate::diag::TERRAIN_HD.load(Ordering::Relaxed)
        );
        match std::fs::write(p, &body) {
            Ok(()) => log!("config saved: remaster={}", enabled()),
            Err(e) => log!("config: could not save {}: {e}", p.display()),
        }
    }
}

fn game_has_focus() -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == unsafe { GetCurrentProcessId() }
}

pub struct Overlay {
    key_down: bool,
    /// Text box with the pack folder (filled from the config on first use).
    path: String,
    path_ready: bool,
    /// The first-run prompt already opened the panel once.
    welcomed: bool,
    /// Last message about the folder, shown under the box.
    note: Option<Note>,
}

/// What the last Apply did (drawn in the overlay language of the moment).
#[derive(Clone, Copy)]
enum Note {
    DefaultFolder,
    Saved,
    NoPack,
}

impl Note {
    fn ok(self) -> bool {
        !matches!(self, Note::NoPack)
    }

    fn text(self) -> &'static str {
        match self {
            Note::DefaultFolder => tr(
                "Back to the default folder. Restart the game to apply.",
                "De volta à pasta padrão. Reinicie o jogo para aplicar.",
            ),
            Note::Saved => tr(
                "Folder saved. Restart the game to load the HD textures.",
                "Pasta salva. Reinicie o jogo para carregar as texturas HD.",
            ),
            Note::NoPack => tr(
                "No HD pack in this folder (pack.idx not found). Nothing changed.",
                "Nenhum pacote HD nesta pasta (pack.idx não encontrado). Nada foi alterado.",
            ),
        }
    }
}

impl Overlay {
    /// Once per frame: the hotkey, and the first-run prompt (opens the panel once when there is no pack).
    pub fn tick(&mut self) {
        self.poll_key();
        if !self.welcomed && NO_PACK.load(Ordering::Relaxed) && !SETUP_DONE.load(Ordering::Relaxed) {
            self.welcomed = true;
            VISIBLE.store(true, Ordering::Relaxed);
        }
    }

    fn poll_key(&mut self) {
        let held = |vk: i32| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0;
        if CAPTURING.load(Ordering::Relaxed) {
            if !game_has_focus() {
                return;
            }
            if held(0x1B) {
                CAPTURING.store(false, Ordering::Relaxed);
                self.key_down = true;
                return;
            }
            // Anything but mouse buttons, modifiers, Esc and the Windows keys.
            let key = (0x08..=0xFEu32).find(|&vk| !matches!(vk, 0x10..=0x12 | 0x1B | 0x5B | 0x5C | 0xA0..=0xA5) && held(vk as i32));
            if let Some(vk) = key {
                let m = (if held(0x10) { MOD_SHIFT } else { 0 }) | (if held(0x11) { MOD_CTRL } else { 0 }) | (if held(0x12) { MOD_ALT } else { 0 });
                KEY.store(vk, Ordering::Relaxed);
                MODS.store(m, Ordering::Relaxed);
                CAPTURING.store(false, Ordering::Relaxed);
                self.key_down = true; // the same press must not close the overlay
                save_settings();
                log!("overlay: hotkey set to {}", hotkey_text());
            }
            return;
        }
        let m = MODS.load(Ordering::Relaxed);
        // modifiers must match exactly, so plain Home (game binding) never opens the overlay
        let mods_ok = held(0x10) == (m & MOD_SHIFT != 0) && held(0x11) == (m & MOD_CTRL != 0) && held(0x12) == (m & MOD_ALT != 0);
        let down = held(KEY.load(Ordering::Relaxed) as i32) && mods_ok;
        if down && !self.key_down && game_has_focus() {
            VISIBLE.fetch_xor(true, Ordering::Relaxed);
        }
        self.key_down = down;
    }

    /// Pack folder section: text box, Browse, Apply. Also the first-run prompt.
    fn folder_section(&mut self, ui: &Ui) {
        if !self.path_ready {
            self.path = PACK_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone();
            self.path_ready = true;
        }
        if let Some(p) = PICKED.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.path = p;
            self.note = None;
        }
        let first_run = NO_PACK.load(Ordering::Relaxed) && !SETUP_DONE.load(Ordering::Relaxed);
        if first_run {
            ui.text_colored(
                [1.0, 0.85, 0.3, 1.0],
                tr("Welcome! Choose where the HD textures are.", "Bem-vindo! Escolha onde estão as texturas HD."),
            );
            ui.text_wrapped(tr(
                "Select the folder with the pack (the one that has pack.idx). Until then, or if you skip, the game shows its original textures.",
                "Escolha a pasta com o pacote (a que tem o pack.idx). Até lá, ou se você pular, o jogo mostra as texturas originais.",
            ));
        } else if NO_PACK.load(Ordering::Relaxed) {
            ui.text_colored(
                [1.0, 0.6, 0.3, 1.0],
                tr(
                    "No HD textures found: the game shows its original textures.",
                    "Nenhuma textura HD encontrada: o jogo mostra as texturas originais.",
                ),
            );
        }
        ui.text(tr("HD texture folder:", "Pasta das texturas HD:"));
        ui.set_next_item_width(-1.0);
        ui.input_text("##packdir", &mut self.path).build();
        if PICKING.load(Ordering::Relaxed) {
            ui.text_disabled(tr("Choose the folder in the dialog window...", "Escolha a pasta na janela que abriu..."));
        } else if ui.button(tr("Browse...###browse", "Procurar...###browse")) {
            start_picker();
        }
        ui.same_line();
        if ui.button(tr("Apply###apply", "Aplicar###apply")) {
            let typed = self.path.trim().trim_matches('"').to_string();
            if typed.is_empty() {
                *PACK_DIR.lock().unwrap_or_else(|e| e.into_inner()) = String::new();
                SETUP_DONE.store(true, Ordering::Relaxed);
                RESTART_NEEDED.store(true, Ordering::Relaxed);
                self.note = Some(Note::DefaultFolder);
                save_settings();
            } else if let Some(dir) = resolve_pack(&typed) {
                let shown = dir.display().to_string();
                self.path = shown.clone();
                *PACK_DIR.lock().unwrap_or_else(|e| e.into_inner()) = shown.clone();
                SETUP_DONE.store(true, Ordering::Relaxed);
                RESTART_NEEDED.store(true, Ordering::Relaxed);
                self.note = Some(Note::Saved);
                save_settings();
                log!("overlay: pack folder set to {shown}");
            } else {
                self.note = Some(Note::NoPack);
            }
        }
        if first_run {
            ui.same_line();
            if ui.button(tr("Skip###skip", "Pular###skip")) {
                SETUP_DONE.store(true, Ordering::Relaxed);
                save_settings();
                log!("overlay: first-run folder selection skipped");
            }
        }
        if let Some(note) = self.note {
            let c = if note.ok() { [0.4, 1.0, 0.4, 1.0] } else { [1.0, 0.4, 0.4, 1.0] };
            ui.text_colored(c, note.text());
        }
        if RESTART_NEEDED.load(Ordering::Relaxed) && self.note.is_none() {
            ui.text_colored(
                [0.4, 1.0, 0.4, 1.0],
                tr("Restart the game to apply the new folder.", "Reinicie o jogo para usar a nova pasta."),
            );
        }
        ui.separator();
    }

    /// The panel's content, without a window around it (the unified overlay puts it in a tab).
    pub fn body(&mut self, ui: &Ui) {
        self.folder_section(ui);
        let mut on = enabled();
        if ui.checkbox(tr("HD textures###hd", "Texturas HD###hd"), &mut on) {
            ENABLED.store(on, Ordering::Relaxed);
            *TOGGLED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
            save_settings();
            crate::reload::request_after(PIECE_SWITCH);
            if !on {
                crate::diag::drop_hd_cache();
            }
            log!("overlay: remaster {}", if on { "ON" } else { "OFF" });
        }
        let mut all = crate::composite::ALL_4X.load(Ordering::Relaxed);
        if ui.checkbox(tr("4x for all players###all4x", "4x para todos os jogadores###all4x"), &mut all) {
            crate::composite::ALL_4X.store(all, Ordering::Relaxed);
            save_settings();
            log!("overlay: 4x for every player {}", if all { "ON" } else { "OFF" });
        }
        ui.text_disabled(tr("HD off shows the original textures.", "Com o HD desligado, o jogo mostra as texturas originais."));
        ui.text_disabled(tr(
            "4x for all players uses more memory and also applies after a map change.",
            "4x para todos os jogadores usa mais memória e também vale depois de trocar de mapa.",
        ));
        ui.separator();
        if crate::reload::available() {
            if ui.button(tr("Reload textures###reload", "Recarregar texturas###reload")) {
                crate::reload::request();
            }
            if crate::reload::BUSY.load(Ordering::Relaxed) {
                ui.same_line();
                ui.text(tr("Reloading...", "Recarregando..."));
            }
            let (chars, texs) = (
                crate::reload::LAST_SCHEDULED.load(Ordering::Relaxed),
                crate::reload::LAST_TEXTURES.load(Ordering::Relaxed),
            );
            ui.text(trf!(
                "Last reload: {chars} characters, {texs} textures",
                "Última recarga: {chars} personagens, {texs} texturas"
            ));
            let mut tint = crate::diag::DEBUG_TINT.load(Ordering::Relaxed);
            if ui.checkbox(tr("Debug colors###tint", "Cores de depuração###tint"), &mut tint) {
                crate::diag::set_debug_tint(tint);
                crate::reload::request_after(std::time::Duration::ZERO);
                log!("overlay: debug colors {}", if tint { "ON" } else { "OFF" });
            }
            if tint {
                if !enabled() {
                    ui.text_colored(
                        [1.0, 0.4, 0.4, 1.0],
                        tr("HD textures are off: nothing is colored.", "As texturas HD estão desligadas: nada é colorido."),
                    );
                }
                ui.text_colored([1.0, 0.0, 1.0, 1.0], tr("Magenta: 2x piece (AI)", "Magenta: peça 2x (IA)"));
                ui.text_colored([0.0, 1.0, 1.0, 1.0], tr("Cyan: 4x piece (AI)", "Ciano: peça 4x (IA)"));
                ui.text_colored(
                    [1.0, 0.65, 0.0, 1.0],
                    tr("Orange: 4x piece, doubled from 2x", "Laranja: peça 4x, dobrada a partir da 2x"),
                );
                ui.text_colored(
                    [1.0, 1.0, 0.0, 1.0],
                    tr("Yellow: plain upscale (no AI)", "Amarelo: ampliação simples (sem IA)"),
                );
                ui.text_colored([0.0, 1.0, 0.0, 1.0], tr("Green: model texture", "Verde: textura de modelo"));
                ui.text_colored(
                    [0.3, 0.3, 1.0, 1.0],
                    tr("Blue: 4x model texture (you)", "Azul: textura de modelo 4x (você)"),
                );
                ui.text(tr("Normal color: not replaced", "Cor normal: não substituída"));
                ui.text(tr("Left alone, by reason:", "Deixadas como estão, por motivo:"));
                ui.text_colored([1.0, 0.25, 0.25, 1.0], tr("Red: effect", "Vermelho: efeito"));
                ui.text_colored(
                    [0.65, 0.25, 0.85, 1.0],
                    tr(
                        "Purple: pair, big member already high resolution (1024 px+)",
                        "Roxo: par, o membro maior já está em alta resolução (1024 px+)",
                    ),
                );
                ui.text_colored(
                    [1.0, 1.0, 1.0, 1.0],
                    tr("White: already at target size", "Branco: já está no tamanho final"),
                );
                ui.text_colored([0.65, 0.4, 0.1, 1.0], tr("Brown: under 128 px", "Marrom: menor que 128 px"));
                ui.text_colored([1.0, 0.7, 0.8, 1.0], tr("Pink: no job, other", "Rosa: sem tarefa, outros"));
                ui.text_colored(
                    [0.6, 0.6, 0.6, 1.0],
                    tr("Gray: character piece source", "Cinza: origem de peça de personagem"),
                );
            }
        } else {
            ui.text_disabled(tr(
                "Live reload unavailable. Change maps to apply.",
                "Recarga ao vivo indisponível. Troque de mapa para aplicar.",
            ));
        }
        ui.separator();
        let pack = PACK_SIZE.load(Ordering::Relaxed);
        ui.text(trf!("HD pack: {pack} textures", "Pacote HD: {pack} texturas"));
        let char_hd = if COMPOSITE_2X.load(Ordering::Relaxed) { tr("on", "ligado") } else { tr("off", "desligado") };
        ui.text(trf!("Character HD: {char_hd}", "HD dos personagens: {char_hd}"));
        let n = crate::composite::N_4X.load(Ordering::Relaxed);
        ui.text(trf!("Characters at 4x: {n}", "Personagens em 4x: {n}"));
        let n = N_PACK.load(Ordering::Relaxed);
        ui.text(trf!("Served from pack: {n}", "Servidas do pacote: {n}"));
        let n = crate::diag::N_CHAR4X.load(Ordering::Relaxed);
        ui.text(trf!("4x textures (you): {n}", "Texturas 4x (você): {n}"));
        let n = N_PLAIN2X.load(Ordering::Relaxed);
        ui.text(trf!("Plain upscales: {n}", "Ampliações simples: {n}"));
        let n = N_REFUSED.load(Ordering::Relaxed);
        ui.text(trf!("Refused resizes: {n}", "Redimensionamentos recusados: {n}"));
        ui.separator();
        crate::mem::refresh_os();
        let o = crate::mem::os();
        let (cache, files, _) = crate::diag::memory_stats();
        let fmt = crate::mem::fmt;
        let (used, budget) = (fmt(o.vram_used), fmt(o.vram_budget));
        ui.text(trf!("VRAM game: {used} of {budget}", "VRAM do jogo: {used} de {budget}"));
        let vram = fmt(crate::mem::mod_vram());
        ui.text(trf!("VRAM mod: ~{vram} (estimate)", "VRAM do mod: ~{vram} (estimativa)"));
        let (working, private) = (fmt(o.ram_working), fmt(o.ram_private));
        ui.text(trf!("RAM game: {working} (private {private})", "RAM do jogo: {working} (privada {private})"));
        let (heap, cache) = (fmt(crate::mem::mod_heap()), fmt(cache as u64));
        ui.text(trf!(
            "RAM mod: {heap} (cache {cache}, {files} files)",
            "RAM do mod: {heap} (cache {cache}, {files} arquivos)"
        ));
        ui.separator();
        if CAPTURING.load(Ordering::Relaxed) {
            ui.text(tr("Press the new hotkey (Esc cancels)", "Aperte a nova tecla de atalho (Esc cancela)"));
        } else {
            let key = hotkey_text();
            ui.text(trf!("Hotkey: {key}", "Tecla de atalho: {key}"));
            ui.same_line();
            if ui.button(tr("Change###hotkey", "Alterar###hotkey")) {
                CAPTURING.store(true, Ordering::Relaxed);
            }
        }
    }

    fn window(&mut self, ui: &mut Ui) {
        let mut open = true;
        ui.window("GW2 Remastered")
            .size([360.0, 0.0], Condition::FirstUseEver)
            .position([60.0, 60.0], Condition::FirstUseEver)
            .opened(&mut open)
            .build(|| self.body(ui));
        if !open {
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }
}

impl ImguiRenderLoop for Overlay {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        ctx.set_ini_filename(None);
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, _render_context: &'a mut dyn RenderContext) {
        let io = ctx.io_mut();
        io.font_global_scale = (io.display_size[1] / 1080.0).clamp(1.0, 2.0);
        io.mouse_draw_cursor = VISIBLE.load(Ordering::Relaxed);
    }

    fn render(&mut self, ui: &mut Ui) {
        self.tick();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if VISIBLE.load(Ordering::Relaxed) {
                self.window(ui);
            }
        }));
        if r.is_err() {
            VISIBLE.store(false, Ordering::Relaxed);
        }
    }

    fn message_filter(&self, _io: &Io) -> MessageFilter {
        if VISIBLE.load(Ordering::Relaxed) { MessageFilter::InputAll } else { MessageFilter::empty() }
    }
}

/// The panel, ready to be drawn by a render loop (the unified mod draws it next to the other features').
pub fn panel() -> Overlay {
    Overlay { key_down: false, path: String::new(), path_ready: false, welcomed: false, note: None }
}

/// Panel waiting for the unified overlay to pick it up (when the DLL is not standalone).
pub static PANEL: std::sync::Mutex<Option<Overlay>> = std::sync::Mutex::new(None);

pub fn install(_module: usize) {
    *PANEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(panel());
    log!("overlay: panel ready for the unified overlay");
}

#[cfg(test)]
mod pack_folder_tests {
    use super::*;

    #[test]
    fn finds_the_pack_in_the_folder_or_its_pack_subfolder() {
        let root = std::env::temp_dir().join(format!("gw2rem_pack_test_{}", std::process::id()));
        let sub = root.join("pack");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(!has_pack(&root) && resolve_pack(root.to_str().unwrap()).is_none());
        std::fs::write(sub.join("pack.idx"), b"x").unwrap();
        assert_eq!(resolve_pack(root.to_str().unwrap()), Some(sub.clone()));
        assert_eq!(resolve_pack(&format!("\"{}\"", sub.display())), Some(sub.clone()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(resolve_pack("").is_none());
    }
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;

    #[test]
    fn parses_and_names_keys() {
        assert_eq!(parse_key("Shift+Home"), Some((0x24, MOD_SHIFT)));
        assert_eq!(parse_key("Ctrl+Alt+F5"), Some((0x74, MOD_CTRL | MOD_ALT)));
        assert_eq!(parse_key("Ctrl+K"), Some((0x4B, MOD_CTRL)));
        assert_eq!(parse_key("7"), Some((0x37, 0)));
        assert_eq!(parse_key("0x24"), Some((0x24, 0)));
        for vk in [0x24u32, 0x70, 0x4B, 0x37, 0xC0] {
            assert_eq!(parse_vk(&key_name(vk)), Some(vk));
        }
    }
}
