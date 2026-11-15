//! The overlay's hotkeys, all in `GW2Enhanced\config.ini`:
//! - `hotkey` (full build only): opens / closes the window on the tab it was left on;
//! - `hotkey_translation`, `hotkey_textures`, `hotkey_graphics`: each feature's own key. In a single-feature build it is
//!   the window's key; in the full build it opens the window straight on that feature's tab (pressed again on that tab:
//!   closes). A feature key equal to the global one acts as the global one.
//!
//! Every key defaults to Shift+Home. Modifiers must match exactly, so the bare key (a game binding) never opens the
//! window. A key is changed in the Version tab: "Change", then press the new combination (Esc cancels).

use std::sync::Mutex;

use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

pub const MOD_SHIFT: u32 = 1;
pub const MOD_CTRL: u32 = 2;
pub const MOD_ALT: u32 = 4;
const VK_HOME: u32 = 0x24;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub vk: u32,
    pub mods: u32,
}

pub const DEFAULT: Key = Key { vk: VK_HOME, mods: MOD_SHIFT };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Global,
    Translation,
    Textures,
    Graphics,
}

impl Target {
    pub const ALL: [Target; 4] = [Target::Global, Target::Translation, Target::Textures, Target::Graphics];

    pub fn ini_key(self) -> &'static str {
        match self {
            Target::Global => "hotkey",
            Target::Translation => "hotkey_translation",
            Target::Textures => "hotkey_textures",
            Target::Graphics => "hotkey_graphics",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The keys of the four targets (index = `Target as usize`).
static KEYS: Mutex<[Key; 4]> = Mutex::new([DEFAULT; 4]);

pub fn get(t: Target) -> Key {
    KEYS.lock().unwrap_or_else(|e| e.into_inner())[t.index()]
}

/// Reads every key from the shared config text (missing or unreadable: Shift+Home).
pub fn load(config: &str) {
    let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    for t in Target::ALL {
        keys[t.index()] = gw2enhanced_core::settings::get(config, t.ini_key()).and_then(|v| parse(&v)).unwrap_or(DEFAULT);
    }
}

/// Sets and saves one key.
pub fn set(t: Target, k: Key) -> std::io::Result<()> {
    KEYS.lock().unwrap_or_else(|e| e.into_inner())[t.index()] = k;
    gw2enhanced_core::settings::save(t.ini_key(), &text(k), Some("Overlay hotkeys (Shift+Home by default; also set in the Version tab)"))
}

/// `Shift+Home`, `Ctrl+Alt+F5`, `0x24` -> key. A key without a modifier is accepted (the player chose it).
pub fn parse(v: &str) -> Option<Key> {
    let mut mods = 0;
    let mut vk = None;
    for part in v.split('+') {
        let p = part.trim();
        match p.to_ascii_lowercase().as_str() {
            "shift" => mods |= MOD_SHIFT,
            "ctrl" | "control" => mods |= MOD_CTRL,
            "alt" => mods |= MOD_ALT,
            _ => vk = Some(parse_vk(p)?),
        }
    }
    vk.map(|vk| Key { vk, mods })
}

fn parse_vk(p: &str) -> Option<u32> {
    let l = p.to_ascii_lowercase();
    if let Some(h) = l.strip_prefix("0x") {
        return u32::from_str_radix(h, 16).ok().filter(|v| (1..=0xFE).contains(v));
    }
    if let Some(n) = l.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then_some(0x6F + n);
    }
    if l.len() == 1 {
        let c = l.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_uppercase() as u32);
        }
    }
    NAMES.iter().find(|(_, n)| n.eq_ignore_ascii_case(p)).map(|(v, _)| *v)
}

const NAMES: &[(u32, &str)] = &[
    (0x08, "Backspace"),
    (0x09, "Tab"),
    (0x0D, "Enter"),
    (0x13, "Pause"),
    (0x14, "CapsLock"),
    (0x20, "Space"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x2C, "PrintScreen"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x91, "ScrollLock"),
    (0xC0, "`"),
];

fn vk_name(vk: u32) -> String {
    if let Some((_, n)) = NAMES.iter().find(|(v, _)| *v == vk) {
        return (*n).to_string();
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("0x{vk:02X}"),
    }
}

pub fn text(k: Key) -> String {
    let mut s = String::new();
    for (bit, name) in [(MOD_CTRL, "Ctrl+"), (MOD_ALT, "Alt+"), (MOD_SHIFT, "Shift+")] {
        if k.mods & bit != 0 {
            s.push_str(name);
        }
    }
    s + &vk_name(k.vk)
}

fn held(vk: u32) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

fn down(k: Key) -> bool {
    held(k.vk)
        && held(0x10) == (k.mods & MOD_SHIFT != 0)
        && held(0x11) == (k.mods & MOD_CTRL != 0)
        && held(0x12) == (k.mods & MOD_ALT != 0)
}

fn game_has_focus() -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == unsafe { GetCurrentProcessId() }
}

/// What a frame's key presses ask the window to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// The global key (or a feature key equal to it, or the only feature's key): open / close.
    Toggle,
    /// A feature key that differs from the global one: open on that tab (close if it is already shown there).
    Tab(Target),
}

/// Edge-triggered polling (call once per frame) plus the capture of a new key.
pub struct Poller {
    was_down: [bool; 4],
    capturing: Option<Target>,
    /// After a capture the keys stay down for a moment: no action until everything is released.
    wait_release: bool,
}

impl Poller {
    pub const fn new() -> Self {
        Poller { was_down: [false; 4], capturing: None, wait_release: false }
    }

    pub fn capturing(&self) -> Option<Target> {
        self.capturing
    }

    pub fn start_capture(&mut self, t: Target) {
        self.capturing = Some(t);
    }

    /// `targets`: the keys that exist in this build (the global one only in the full build).
    pub fn poll(&mut self, targets: &[Target], full_build: bool) -> Option<Action> {
        if !game_has_focus() {
            return None;
        }
        if let Some(t) = self.capturing {
            if held(0x1B) {
                self.capturing = None;
                self.wait_release = true;
                return None;
            }
            let vk = (0x08..=0xFEu32).find(|&vk| !matches!(vk, 0x10..=0x12 | 0x1B | 0x5B | 0x5C | 0xA0..=0xA5) && held(vk));
            if let Some(vk) = vk {
                let mods = (if held(0x10) { MOD_SHIFT } else { 0 })
                    | (if held(0x11) { MOD_CTRL } else { 0 })
                    | (if held(0x12) { MOD_ALT } else { 0 });
                let k = Key { vk, mods };
                if let Err(e) = set(t, k) {
                    gw2enhanced_core::logs::HOST.write(&format!("hotkey: could not save {}: {e}", t.ini_key()));
                }
                gw2enhanced_core::logs::HOST.write(&format!("hotkey: {} = {}", t.ini_key(), text(k)));
                self.capturing = None;
                self.wait_release = true;
            }
            return None;
        }
        let global = get(Target::Global);
        let mut action = None;
        let mut any = false;
        for &t in targets {
            let k = get(t);
            let d = down(k);
            any |= d;
            let pressed = d && !self.was_down[t.index()];
            self.was_down[t.index()] = d;
            if !pressed || self.wait_release || action.is_some() {
                continue;
            }
            action = Some(if t == Target::Global || !full_build || k == global { Action::Toggle } else { Action::Tab(t) });
        }
        if self.wait_release {
            self.wait_release = any || held(0x10) || held(0x11) || held(0x12);
            return None;
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_and_print_the_same() {
        for s in ["Shift+Home", "Ctrl+Alt+F5", "Insert", "Shift+End", "Ctrl+K", "Alt+0", "0x91", "Shift+PageUp"] {
            let k = parse(s).unwrap_or_else(|| panic!("{s}"));
            let back = text(k);
            assert_eq!(parse(&back), Some(k), "{s} -> {back}");
        }
        assert_eq!(parse("Shift+Home"), Some(DEFAULT));
        assert_eq!(text(parse("ctrl+shift+end").unwrap()), "Ctrl+Shift+End");
        assert_eq!(parse("0x24"), Some(Key { vk: 0x24, mods: 0 }));
        assert!(parse("Shift").is_none() && parse("Shift+Nope").is_none() && parse("").is_none() && parse("F25").is_none());
    }

    #[test]
    fn missing_keys_default_to_shift_home() {
        load("hotkey_graphics=Ctrl+F9\nhotkey_textures=garbage\n");
        assert_eq!(get(Target::Global), DEFAULT);
        assert_eq!(get(Target::Textures), DEFAULT);
        assert_eq!(get(Target::Graphics), Key { vk: 0x78, mods: MOD_CTRL });
        load("");
    }
}
