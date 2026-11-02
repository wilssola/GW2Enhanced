//! Speech bubbles over characters (`AsChatBubble.cpp`).
//!
//! A bubble is filled once (`set_content`, slot 0 of its vtable, RVA
//! `0x3bcad0`) with a coded text ([`crate::coded`]): a format string plus the
//! string ID of the dialogue line. The text control decodes it right away, so
//! a line shown before its translation existed stays in English.
//!
//! Bubbles whose line had no translation are remembered with a copy of their
//! coded text. When the translation exists and the text cache was refreshed
//! after it ([`crate::refresh::count`]), the same coded text is set again with
//! the game's own "set text" (`0x1048020`, control message `0x5d`): the
//! control decodes it again, now translated by `F_materialize`.
//!
//! That only happens inside the bubble's message handler (`0x3bbbe0`), right
//! after the game handled a message for that bubble, the same context in which
//! the game fills it. Message 0xE destroys the bubble.
//!
//! While a bubble waits for its translation it is kept on screen a little
//! longer, in proportion to the length of its English line (at most
//! `bubble_wait` seconds), and once translated it gets enough time to be read: `+0x60` is the time left before it fades out, counted down
//! every frame while `+0x70` (state) is 1 = shown (`0x3bc210`).
//!
//! Bubble object: `+0x00` vtable, `+0x18` control handle; its first child
//! (`FUN_141071650(handle, 0)`) is the text control. Content: `+0x18` coded text.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use crate::game::Image;
use crate::{coded, hooks, log, sig};

/// `fn(bubble, content)`.
type SetContentFn = unsafe extern "system" fn(*mut u8, *const u8) -> u64;
/// `fn(msg, a, b)`: `msg+4` = type, `*(msg+8)` = address of the object pointer.
type HandlerFn = unsafe extern "system" fn(*mut u8, *mut u8, *mut u8) -> u64;
/// `fn(control, coded text)`.
type SetTextFn = unsafe extern "system" fn(u32, *const u16) -> u64;
/// `fn(control, index) -> child control`.
type ChildFn = unsafe extern "system" fn(u32, u32) -> u32;

const BUBBLE_HANDLE: usize = 0x18;
/// Seconds left before the bubble fades out, and its state (1 = shown).
const BUBBLE_TIME_LEFT: usize = 0x60;
const BUBBLE_STATE: usize = 0x70;
const STATE_SHOWN: i32 = 1;
/// Time kept on screen at least, while waiting for a translation.
const HOLD_MARGIN: f32 = 0.5;
/// Extra time while waiting: base + per English character (capped by `bubble_wait`).
const WAIT_BASE: f32 = 1.0;
const WAIT_PER_CHAR: f32 = 1.0 / 20.0;
/// Reading time given to a line that was just translated: base + per character, capped.
const READ_BASE: f32 = 2.0;
const READ_PER_CHAR: f32 = 1.0 / 15.0;
const READ_MAX: f32 = 12.0;
const CONTENT_TEXT: usize = 0x18;
const MSG_TYPE: usize = 0x04;
const MSG_OBJECT_SLOT: usize = 0x08;
const MSG_CREATE: u32 = 0x0A;
const MSG_DESTROY: u32 = 0x0E;
/// Bubbles remembered at most (oldest dropped first).
const MAX_WAITING: usize = 256;

static SET_CONTENT: OnceLock<GenericDetour<SetContentFn>> = OnceLock::new();
static HANDLER: OnceLock<GenericDetour<HandlerFn>> = OnceLock::new();
static SET_TEXT: OnceLock<SetTextFn> = OnceLock::new();
static CHILD: OnceLock<ChildFn> = OnceLock::new();

struct Waiting {
    bubble: usize,
    /// The bubble's coded text, ended by `0x0000`.
    coded: Vec<u16>,
    /// String IDs of the dialogue line.
    lines: Vec<u32>,
    /// [`crate::live::version`] at the last check.
    checked: u64,
    /// Refresh count when the translation was found: the text is set again
    /// after the next refresh (the cache then no longer has the English).
    ready_at: Option<u64>,
    /// Extra seconds already given while waiting.
    held: f32,
    /// Extra seconds this bubble may get while waiting (from its length).
    max_hold: f32,
}

/// Extra seconds a line of `chars` English characters may wait, capped at `cap`.
fn wait_for(chars: usize, cap: f32) -> f32 {
    (WAIT_BASE + chars as f32 * WAIT_PER_CHAR).min(cap)
}

static WAITING: Mutex<Vec<Waiting>> = Mutex::new(Vec::new());
/// Coded texts handed to the game, kept alive until the process ends (the
/// control may keep the pointer).
static KEPT: Mutex<Vec<Box<[u16]>>> = Mutex::new(Vec::new());
static SAMPLES: AtomicU64 = AtomicU64::new(0);
/// Bubbles logged per session.
const MAX_LOGGED: u64 = 200;
static REPLACED: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn set_content_hook(bubble: *mut u8, content: *const u8) -> u64 {
    let ret = unsafe { SET_CONTENT.get().unwrap_unchecked().call(bubble, content) };
    if !bubble.is_null() && !content.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe { remember(bubble as usize, content) });
    }
    ret
}

/// The string `id` needs nothing more: it has a translation, or it is only
/// markup (`[f]%str1%`, the speaker's format).
fn settled(id: u32) -> bool {
    hooks::lookup_id(id).is_some() || hooks::english_of(id).is_some_and(|en| !has_words(&en))
}

/// Has letters outside the game's markup (`[s]`, `%str1%`, `<c=…>`).
fn has_words(text: &str) -> bool {
    let mut depth: Option<char> = None;
    for c in text.chars() {
        match (depth, c) {
            (None, '[') => depth = Some(']'),
            (None, '<') => depth = Some('>'),
            (None, '%') => depth = Some('%'),
            (Some(end), c) if c == end => depth = None,
            (None, c) if c.is_alphabetic() => return true,
            _ => {}
        }
    }
    false
}

/// A bubble on screen is waiting for the translation of the string `id`.
pub fn waiting_for(id: u32) -> bool {
    WAITING.lock().is_ok_and(|w| w.iter().any(|x| x.lines.contains(&id)))
}

unsafe fn remember(bubble: usize, content: *const u8) {
    if coded::readable(content as usize) < CONTENT_TEXT + 8 {
        return;
    }
    let p = unsafe { content.add(CONTENT_TEXT).cast::<*const u16>().read() };
    if p.is_null() || !(p as usize).is_multiple_of(2) {
        return;
    }
    let Some(coded) = (unsafe { coded::copy(p, coded::readable(p as usize) / 2) }) else {
        return;
    };
    let ids = coded::string_ids(&coded);
    // The first ID is the format ("speaker: line"); the line comes after it.
    let lines: Vec<u32> = if ids.len() > 1 { ids[1..].to_vec() } else { ids };
    let missing = !lines.iter().all(|&id| settled(id));
    // The next lines of the conversation usually follow in ID order: translate them now.
    if let Some(c) = crate::STATE.get().and_then(|s| s.collector.as_ref()) {
        c.send(crate::collect::Item::Upcoming(lines.clone()));
    }
    if SAMPLES.fetch_add(1, Ordering::Relaxed) < MAX_LOGGED {
        let english: Vec<String> = lines
            .iter()
            .map(|&id| hooks::english_of(id).map_or("?".into(), |e| e.chars().take(40).collect()))
            .collect();
        log!(
            "bubble: lines {lines:?} {english:?}, translation missing: {missing} [{}]",
            coded::hex(&coded)
        );
    }
    if let Ok(mut w) = WAITING.lock() {
        w.retain(|x| x.bubble != bubble);
        if missing && !lines.is_empty() && hooks::english_client() {
            if w.len() >= MAX_WAITING {
                w.remove(0);
            }
            let cap = crate::runtime::bubble_wait() as f32;
            let english: Option<Vec<String>> = lines.iter().map(|&id| hooks::english_of(id)).collect();
            // Text still encrypted (its key comes later): the length is unknown,
            // so it may wait as long as allowed.
            let max_hold = match english {
                Some(texts) => wait_for(texts.iter().map(|t| t.chars().count()).sum(), cap),
                None => cap,
            };
            w.push(Waiting {
                bubble,
                coded,
                lines,
                checked: u64::MAX,
                ready_at: None,
                held: 0.0,
                max_hold,
            });
        }
    }
}

unsafe extern "system" fn handler_hook(msg: *mut u8, a: *mut u8, b: *mut u8) -> u64 {
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
        if let Ok(mut w) = WAITING.lock() {
            w.retain(|x| x.bubble != obj);
        }
    }
    let ret = unsafe { HANDLER.get().unwrap_unchecked().call(msg, a, b) };
    if !msg.is_null() && kind != MSG_CREATE && kind != MSG_DESTROY {
        let _ = std::panic::catch_unwind(|| unsafe {
            let obj = object();
            hold(obj);
            apply(obj);
        });
    }
    ret
}

/// Time left / state of a bubble.
unsafe fn time_left(bubble: usize) -> (*mut f32, i32) {
    let b = bubble as *mut u8;
    unsafe {
        (
            b.add(BUBBLE_TIME_LEFT).cast::<f32>(),
            b.add(BUBBLE_STATE).cast::<i32>().read(),
        )
    }
}

/// Keeps a waiting bubble from fading out, up to `bubble_wait` extra seconds.
unsafe fn hold(bubble: usize) {
    if bubble == 0 {
        return;
    }
    let Ok(mut w) = WAITING.lock() else { return };
    let Some(x) = w.iter_mut().find(|x| x.bubble == bubble) else {
        return;
    };
    let (left, state) = unsafe { time_left(bubble) };
    if state != STATE_SHOWN || x.held >= x.max_hold {
        return;
    }
    let now = unsafe { left.read() };
    if now.is_finite() && now < HOLD_MARGIN {
        let add = HOLD_MARGIN - now.max(0.0);
        unsafe { left.write(HOLD_MARGIN) };
        if x.held == 0.0 {
            log!(
                "bubble: held on screen while its translation is made (up to {:.1}s)",
                x.max_hold
            );
        }
        x.held += add;
    }
}

/// Gives a bubble that was just translated enough time to be read.
unsafe fn give_reading_time(bubble: usize, chars: usize) {
    let (left, state) = unsafe { time_left(bubble) };
    if state != STATE_SHOWN {
        return;
    }
    let want = (READ_BASE + chars as f32 * READ_PER_CHAR).min(READ_MAX);
    let now = unsafe { left.read() };
    if now.is_finite() && now < want {
        unsafe { left.write(want) };
    }
}

/// Sets the coded text of `bubble` again once its line can be translated.
unsafe fn apply(bubble: usize) {
    if bubble == 0 {
        return;
    }
    let version = crate::live::version();
    let refreshes = crate::refresh::count();
    let (coded, chars) = {
        let Ok(mut w) = WAITING.lock() else { return };
        let Some(x) = w.iter_mut().find(|x| x.bubble == bubble) else {
            return;
        };
        if x.ready_at.is_none() {
            // Shown translated already (its text was decoded after the check in `remember`).
            if x.lines.iter().all(|&id| hooks::in_dictionary(id)) {
                w.retain(|x| x.bubble != bubble);
                return;
            }
            if x.checked == version {
                return;
            }
            x.checked = version;
            if !x.lines.iter().all(|&id| settled(id)) {
                return;
            }
            x.ready_at = Some(refreshes);
            // The text is set again after the next refresh: do it now.
            crate::refresh::request_now();
        }
        if x.ready_at.is_some_and(|at| refreshes <= at) {
            return;
        }
        let chars: usize = x
            .lines
            .iter()
            .filter_map(|&id| hooks::lookup_id(id))
            .map(|t| t.chars().count())
            .sum();
        (x.coded.clone(), chars)
    };
    let (Some(set_text), Some(child)) = (SET_TEXT.get(), CHILD.get()) else {
        return;
    };
    let handle = unsafe { (bubble as *const u8).add(BUBBLE_HANDLE).cast::<u32>().read() };
    // Handle 0 would make the game pick another control.
    if handle == 0 {
        return;
    }
    let text_control = unsafe { child(handle, 0) };
    if text_control == 0 {
        return;
    }
    let Ok(mut kept) = KEPT.lock() else { return };
    kept.push(coded.into_boxed_slice());
    let ptr = kept.last().map_or(std::ptr::null(), |c| c.as_ptr());
    drop(kept);
    unsafe { set_text(text_control, ptr) };
    if crate::runtime::bubble_wait() > 0 {
        unsafe { give_reading_time(bubble, chars) };
    }
    if let Ok(mut w) = WAITING.lock() {
        w.retain(|x| x.bubble != bubble);
    }
    let n = REPLACED.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 10 || n.is_multiple_of(100) {
        log!("bubble: {n} replaced after translation arrived ({chars} chars)");
    }
}

pub fn install(img: &Image) {
    let (Some(set_content), Some(handler), Some(set_text), Some(child)) = (
        img.find(&sig::BUBBLE_SET_CONTENT),
        img.find(&sig::BUBBLE_HANDLER),
        img.find(&sig::CTL_SET_TEXT),
        img.find(&sig::CTL_CHILD),
    ) else {
        log!("bubble: signatures NOT found — late bubble translation disabled");
        return;
    };
    let _ = SET_TEXT.set(unsafe { std::mem::transmute::<usize, SetTextFn>(set_text) });
    let _ = CHILD.set(unsafe { std::mem::transmute::<usize, ChildFn>(child) });
    hooks::attach(
        img,
        sig::BUBBLE_SET_CONTENT.name,
        set_content,
        set_content_hook as SetContentFn,
        &SET_CONTENT,
    );
    hooks::attach(
        img,
        sig::BUBBLE_HANDLER.name,
        handler,
        handler_hook as HandlerFn,
        &HANDLER,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_longer_for_longer_lines_up_to_the_cap() {
        assert_eq!(wait_for(0, 8.0), 1.0);
        assert_eq!(wait_for(60, 8.0), 4.0);
        assert_eq!(wait_for(400, 8.0), 8.0);
        assert_eq!(wait_for(60, 0.0), 0.0);
    }

    #[test]
    fn markup_alone_has_no_words() {
        assert!(!has_words("[f]%str1%"));
        assert!(!has_words("<c=@reminder>%num1%</c>"));
        assert!(has_words("Glint"));
        assert!(has_words("%str1%, are you ready?"));
    }
}
