//! The game's chat (`ChatLog.cpp` / `ChatLogEntry.cpp`).
//!
//! Each chat tab stores the messages and builds the lines from them;
//! `rebuild` (`0x43bfc0`) clears the lines and builds everything again. An
//! NPC line keeps a reference to the string, not the text: rebuilt after a
//! refresh, it is decoded again and comes out translated.
//!
//! Tabs are tracked through the control's message handler
//! (`0x43b6b0`): message 10 creates the object and 0xE destroys it. Only
//! live tabs are rebuilt.
//!
//! A rebuild destroys and recreates the tab's line controls, so it only runs
//! inside that tab's own message handler, right after the game handled an
//! update or layout message (the game rebuilds from its handler too). Running
//! it from the text cache tick, in the middle of another control's layout,
//! crashed the game.
//!
//! The rebuild asserts that the tab has no lines left (`m_count == 0`) after
//! clearing them, and the count only drops when the tab is notified that each
//! line was destroyed. Inside a notification (message 0x31) those arrive
//! later, so the game aborted. The tab is therefore cleared first and only
//! rebuilt once its count is 0.
//!
//! The default tabs get their name through the text system, but the game saves
//! tabs in `Local.dat` with the name as plain text, so tabs saved before the
//! translation keep their English name. Those names are translated as the tabs are
//! added. The game then saves the translated name, like any renamed tab.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use retour::GenericDetour;

use crate::game::Image;
use crate::{hooks, log, sig};

/// `fn(msg, a, b)`: `msg+4` = type, `*(msg+8)` = address of the object pointer.
type DispatchFn = unsafe extern "system" fn(*mut u8, *mut u8, *mut u8) -> u64;
type RebuildFn = unsafe extern "system" fn(*mut u8);
/// `fn(manager, tab)`.
type AddTabFn = unsafe extern "system" fn(*mut u8, *mut u8) -> *mut u8;
/// `fn(tab, name)`.
type SetNameFn = unsafe extern "system" fn(*mut u8, *const u16);
/// `fn(control, message, a, b)`.
type SendFn = unsafe extern "system" fn(u32, u32, *const u8, *const u8) -> u64;
/// `fn(control, index) -> child control`.
type ChildFn = unsafe extern "system" fn(u32, u32) -> u32;

const MSG_TYPE: usize = 0x04;
const MSG_OBJECT_SLOT: usize = 0x08;
const MSG_CREATE: u32 = 0x0A;
const MSG_DESTROY: u32 = 0x0E;
/// Messages after which a tab may be rebuilt: update and layout (never inside
/// a notification).
const REBUILD_ON: [u32; 2] = [0x08, 0x37];
/// Chat tab: control handle and number of lines (`m_count`).
const LOG_HANDLE: usize = 0x18;
const LOG_COUNT: usize = 0x68;
/// Control message that removes every line of the list.
const MSG_CLEAR: u32 = 0x4C;
/// Tab: name, a wide string (pointer to its characters).
const TAB_NAME: usize = 0x10;
/// Longest tab name, in characters (the game asserts on longer ones).
const TAB_NAME_MAX: usize = 15;
/// Default tab names (`ChatInt`: text IDs 0xbcdf and 0xc127).
const DEFAULT_TABS: [&str; 2] = ["Main", "Combat"];
/// Minimum interval between rebuilds.
const REBUILD_INTERVAL_MS: u64 = 2000;

static DISPATCH: OnceLock<GenericDetour<DispatchFn>> = OnceLock::new();
static ADD_TAB: OnceLock<GenericDetour<AddTabFn>> = OnceLock::new();
/// Offset of `set_name` in the tab's vtable, read from the game's own call.
static SET_NAME_SLOT: OnceLock<usize> = OnceLock::new();
static REBUILD: OnceLock<RebuildFn> = OnceLock::new();
static SEND: OnceLock<SendFn> = OnceLock::new();
static CHILD: OnceLock<ChildFn> = OnceLock::new();
static POSTPONED: AtomicU64 = AtomicU64::new(0);

/// Live chat tabs.
static LOGS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static PENDING: AtomicBool = AtomicBool::new(false);
/// Tabs waiting for a rebuild (done in their message handler).
static DIRTY: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Set while a tab is being rebuilt (the rebuild sends messages of its own).
static REBUILDING: AtomicBool = AtomicBool::new(false);
/// Message types seen by tracked tabs (logged once each, for diagnosis).
static KINDS_SEEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static START: OnceLock<Instant> = OnceLock::new();
static LAST_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

unsafe extern "system" fn dispatch_hook(msg: *mut u8, a: *mut u8, b: *mut u8) -> u64 {
    let kind = if msg.is_null() {
        0
    } else {
        unsafe { msg.add(MSG_TYPE).cast::<u32>().read() }
    };
    let object = || unsafe {
        let slot = msg.add(MSG_OBJECT_SLOT).cast::<*const usize>().read();
        if slot.is_null() { 0 } else { slot.read() }
    };
    if kind == MSG_DESTROY {
        let obj = object();
        if let Ok(mut logs) = LOGS.lock() {
            logs.retain(|&o| o != obj);
        }
        if let Ok(mut dirty) = DIRTY.lock() {
            dirty.retain(|&o| o != obj);
        }
    }
    let ret = unsafe { DISPATCH.get().unwrap_unchecked().call(msg, a, b) };
    if kind != MSG_CREATE && kind != MSG_DESTROY && !msg.is_null() {
        let _ = std::panic::catch_unwind(|| rebuild_if_dirty(object(), kind));
    }
    if kind == MSG_CREATE {
        let obj = object();
        if obj != 0
            && let Ok(mut logs) = LOGS.lock()
            && !logs.contains(&obj)
        {
            logs.push(obj);
        }
    }
    ret
}

/// The name to give a saved tab: only an English default name that has a
/// translation that fits. A name the player chose is never touched.
fn tab_translation(name: &str, translate: impl Fn(&str) -> Option<String>) -> Option<Vec<u16>> {
    if !DEFAULT_TABS.contains(&name) {
        return None;
    }
    let tr = translate(name).filter(|t| t != name && t.chars().count() <= TAB_NAME_MAX)?;
    Some(tr.encode_utf16().chain([0]).collect())
}

unsafe fn tab_name(tab: *const u8) -> Option<String> {
    let text = unsafe { tab.add(TAB_NAME).cast::<*const u16>().read() };
    if text.is_null() {
        return None;
    }
    let chars: Vec<u16> = (0..=TAB_NAME_MAX)
        .map(|i| unsafe { text.add(i).read() })
        .take_while(|&c| c != 0)
        .collect();
    (chars.len() <= TAB_NAME_MAX).then(|| String::from_utf16_lossy(&chars))
}

unsafe fn translate_tab(tab: *mut u8) {
    let (Some(&slot), Some(state)) = (SET_NAME_SLOT.get(), crate::STATE.get()) else {
        return;
    };
    let Some(name) = (unsafe { tab_name(tab) }) else { return };
    let Some(new) = tab_translation(&name, |en| state.dict.get(en).map(str::to_owned)) else {
        return;
    };
    unsafe {
        let vtable = tab.cast::<*const u8>().read();
        let set: SetNameFn = std::mem::transmute(vtable.add(slot).cast::<usize>().read());
        set(tab, new.as_ptr());
    }
    log!("chat: saved tab {name:?} translated");
}

unsafe extern "system" fn add_tab_hook(manager: *mut u8, tab: *mut u8) -> *mut u8 {
    let ret = unsafe { ADD_TAB.get().unwrap_unchecked().call(manager, tab) };
    if !tab.is_null() && crate::runtime::get(crate::runtime::Flag::Translate) {
        let _ = std::panic::catch_unwind(|| unsafe { translate_tab(tab) });
    }
    ret
}

/// Rebuilds `obj` if it is a live tab waiting for a rebuild. Called from the
/// tab's message handler, after the game handled the message.
fn rebuild_if_dirty(obj: usize, kind: u32) {
    if obj == 0 || REBUILDING.load(Ordering::Relaxed) {
        return;
    }
    if !LOGS.lock().is_ok_and(|l| l.contains(&obj)) {
        return;
    }
    if let Ok(mut seen) = KINDS_SEEN.lock()
        && seen.len() < 64
        && !seen.contains(&kind)
    {
        seen.push(kind);
        log!("chat: tab message {kind:#x}");
    }
    if !REBUILD_ON.contains(&kind) || !DIRTY.lock().is_ok_and(|d| d.contains(&obj)) {
        return;
    }
    let (Some(rebuild), Some(send), Some(child)) = (REBUILD.get(), SEND.get(), CHILD.get()) else {
        return;
    };
    let tab = obj as *const u8;
    let handle = unsafe { tab.add(LOG_HANDLE).cast::<u32>().read() };
    // Handle 0 would make the game pick another control.
    if handle == 0 {
        return;
    }
    let list = unsafe { child(handle, 0) };
    if list == 0 {
        return;
    }
    let count = || unsafe { tab.add(LOG_COUNT).cast::<i32>().read() };
    REBUILDING.store(true, Ordering::Relaxed);
    if count() != 0 {
        unsafe { send(list, MSG_CLEAR, std::ptr::null(), std::ptr::null()) };
    }
    if count() != 0 {
        // Lines still being removed: try again on a later message.
        REBUILDING.store(false, Ordering::Relaxed);
        if POSTPONED.fetch_add(1, Ordering::Relaxed) < 10 {
            log!("chat: rebuild postponed ({} lines still being removed)", count());
        }
        return;
    }
    if let Ok(mut d) = DIRTY.lock() {
        d.retain(|&o| o != obj);
    }
    unsafe { rebuild(obj as *mut u8) };
    REBUILDING.store(false, Ordering::Relaxed);
    log!("chat: tab rebuilt (on message {kind:#x})");
}

/// Marks every chat tab for a rebuild; each one is rebuilt the next time the
/// game sends it a message.
pub fn rebuild_all() {
    let logs: Vec<usize> = LOGS.lock().map(|l| l.clone()).unwrap_or_default();
    if let Ok(mut dirty) = DIRTY.lock() {
        for obj in logs {
            if !dirty.contains(&obj) {
                dirty.push(obj);
            }
        }
    }
    LAST_MS.store(now_ms(), Ordering::Relaxed);
}

/// Rebuilds the chat tabs on the next frame (at most every
/// [`REBUILD_INTERVAL_MS`]), e.g. after a line on screen was translated.
pub fn request_rebuild() {
    PENDING.store(true, Ordering::Relaxed);
}

/// Called every frame by the refresh: applies translations that have arrived.
pub fn on_tick() {
    if now_ms().saturating_sub(LAST_MS.load(Ordering::Relaxed)) >= REBUILD_INTERVAL_MS
        && PENDING.swap(false, Ordering::Relaxed)
    {
        rebuild_all();
    }
}

/// Hooks the tab manager, so saved tabs get a translated name.
fn install_tab_names(img: &Image) {
    let (Some(add), Some(call)) = (img.find(&sig::CHAT_ADD_TAB), img.find(&sig::CHAT_DEFAULT_NAME)) else {
        log!("chat: tab signatures NOT found — saved tabs keep their names");
        return;
    };
    // `call [rax + disp32]`, 13 bytes into the signature.
    let slot = unsafe { ((call + 13) as *const u32).read_unaligned() } as usize;
    let _ = SET_NAME_SLOT.set(slot);
    hooks::attach(img, sig::CHAT_ADD_TAB.name, add, add_tab_hook as AddTabFn, &ADD_TAB);
}

pub fn install(img: &Image) {
    install_tab_names(img);
    let (Some(dispatch), Some(rebuild), Some(send), Some(child)) = (
        img.find(&sig::CHAT_DISPATCH),
        img.find(&sig::CHAT_REBUILD),
        img.find(&sig::CTL_SEND),
        img.find(&sig::CTL_CHILD),
    ) else {
        log!("chat: signatures NOT found — chat lines are not refreshed");
        return;
    };
    START.get_or_init(Instant::now);
    let _ = REBUILD.set(unsafe { std::mem::transmute::<usize, RebuildFn>(rebuild) });
    let _ = SEND.set(unsafe { std::mem::transmute::<usize, SendFn>(send) });
    let _ = CHILD.set(unsafe { std::mem::transmute::<usize, ChildFn>(child) });
    hooks::attach(
        img,
        sig::CHAT_DISPATCH.name,
        dispatch,
        dispatch_hook as DispatchFn,
        &DISPATCH,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(en: &str) -> Option<String> {
        match en {
            "Main" => Some("Principal".into()),
            "Combat" => Some("Combate".into()),
            "Other" => Some("Outros".into()),
            _ => None,
        }
    }

    fn text(v: Option<Vec<u16>>) -> Option<String> {
        v.map(|v| String::from_utf16_lossy(&v[..v.len() - 1]))
    }

    #[test]
    fn saved_default_tabs_are_translated() {
        assert_eq!(text(tab_translation("Main", pt)).as_deref(), Some("Principal"));
        assert_eq!(text(tab_translation("Combat", pt)).as_deref(), Some("Combate"));
    }

    #[test]
    fn names_the_player_chose_are_kept() {
        assert_eq!(tab_translation("Other", pt), None);
        assert_eq!(tab_translation("Guilda", pt), None);
        assert_eq!(tab_translation("Principal", pt), None);
    }

    #[test]
    fn a_translation_longer_than_the_game_allows_is_refused() {
        assert_eq!(
            tab_translation("Main", |_| Some("Uma tradução longa demais".into())),
            None
        );
        assert_eq!(tab_translation("Main", |_| None), None);
        assert_eq!(tab_translation("Main", |_| Some("Main".into())), None);
    }
}
