//! Hooks into the game's text system (`TextDecode.cpp`).
//!
//! `F_materialize(rcx, ctx, id, entry, keySeed)` (RVA `0x10e0840`)
//! decodes a string, including RC4-encrypted ones, into a `TArray<u16>`
//! in the context and returns its pointer. Right after that the
//! `CParser` (plurals, `%str1%`, ...) reads from that array. By replacing the
//! array's contents here, using the game's own `Reserve`, the translation can
//! have any length and the memory still belongs to the game.
//!
//! For `seed_decode`, `reg-insert` delivers the encrypted entries the game
//! puts in the cache and `bulk_insert` the `keySeed`s the server sends.
//!
//! Field offsets are not fixed: they come from [`layout`].

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::{Function, GenericDetour};

use crate::collect::Item;
use crate::game::Image;
use crate::layout::{self, Bulk, Text};
use crate::runtime::{self, Flag};
use crate::sig::{self, MATERIALIZE};
use crate::{log, rc4};

/// `fn(rcx, ctx, id, entry, keySeed) -> *u16`
type MaterializeFn = unsafe extern "system" fn(*mut u8, *mut u8, u32, *const u8, u64) -> *mut u16;
/// The game's `TArray<u16>::Reserve(arr, units)`.
type ReserveFn = unsafe extern "system" fn(*mut u8, u32);
/// `fn(cache, a, textId, entry)`
type RegInsertFn = unsafe extern "system" fn(*mut u8, u32, u32, *const u8) -> u64;
/// `fn(rcx, msg)`; message format in [`Bulk`].
type BulkInsertFn = unsafe extern "system" fn(*mut u8, *const u8) -> u32;

static DETOUR: OnceLock<GenericDetour<MaterializeFn>> = OnceLock::new();
static REG_DETOUR: OnceLock<GenericDetour<RegInsertFn>> = OnceLock::new();
static BULK_DETOUR: OnceLock<GenericDetour<BulkInsertFn>> = OnceLock::new();
static TEXT: OnceLock<Text> = OnceLock::new();
static BULK: OnceLock<Bulk> = OnceLock::new();

static REG_SAMPLES: AtomicU64 = AtomicU64::new(0);
static BULK_SAMPLES: AtomicU64 = AtomicU64::new(0);
static SEEN: AtomicU64 = AtomicU64::new(0);
static TRANSLATED: AtomicU64 = AtomicU64::new(0);
static CODED: AtomicU64 = AtomicU64::new(0);
static BY_ID: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn materialize_hook(a: *mut u8, ctx: *mut u8, id: u32, entry: *const u8, seed: u64) -> *mut u16 {
    // The hook is only enabled after DETOUR has been filled.
    let ret = unsafe { DETOUR.get().unwrap_unchecked().call(a, ctx, id, entry, seed) };
    if ret.is_null() || ctx.is_null() {
        return ret;
    }
    // A panic here would crash the game: on error, return the original.
    std::panic::catch_unwind(AssertUnwindSafe(|| unsafe { on_text(ctx, id, entry, ret) })).unwrap_or(ret)
}

unsafe fn on_text(ctx: *mut u8, id: u32, entry: *const u8, ret: *mut u16) -> *mut u16 {
    let (Some(state), Some(t)) = (crate::STATE.get(), TEXT.get()) else {
        return ret;
    };
    let arr = unsafe { ctx.add(t.arr) };
    let ptr = unsafe { arr.add(t.ptr).cast::<*mut u16>().read() };
    let count = unsafe { arr.add(t.count).cast::<u32>().read() } as usize;
    if ptr != ret || count == 0 {
        return ret;
    }
    let units = unsafe { std::slice::from_raw_parts(ptr, count) };
    let end = units.iter().position(|&u| u == 0).unwrap_or(count);
    let Ok(original) = String::from_utf16(&units[..end]) else {
        return ret;
    };

    let n = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 15 {
        let sample: String = original.chars().take(50).collect();
        log!("text #{n}: id={id} {sample:?}");
    }
    if english_client() {
        remember_english(id, &original);
    }
    let coded = !entry.is_null() && rc4::Header::peek(unsafe { &*entry.cast::<[u8; rc4::HEADER]>() }).is_protected();
    let english = english_client();
    if coded {
        CODED.fetch_add(1, Ordering::Relaxed);
        if let Some(c) = state
            .collector
            .as_ref()
            .filter(|_| runtime::get(Flag::SeedDecode) && english)
        {
            c.send(Item::Seed(id, original.clone()));
        }
    }

    // By the original text; otherwise what was translated live; otherwise by ID
    // (the latter works with the game in any language).
    let translate = runtime::get(Flag::Translate);
    let live = if translate && state.dict.get(&original).is_none() {
        crate::live::get(&original)
    } else {
        None
    };
    let pt = if translate {
        state.dict.get(&original).or(live.as_deref()).or_else(|| {
            let pt = state.dict.get_id(id);
            if pt.is_some() {
                BY_ID.fetch_add(1, Ordering::Relaxed);
            }
            pt
        })
    } else {
        None
    };
    // Never replace the original text with `[null]`.
    let pt = pt.filter(|pt| !crate::dict::unusable(&original, pt));
    let out = match pt.and_then(|pt| unsafe { write_text(t, arr, pt) }) {
        Some(p) => {
            TRANSLATED.fetch_add(1, Ordering::Relaxed);
            p
        }
        None => {
            if pt.is_none() && english {
                // Dialogue (story scenes, bubbles) usually continues with the next
                // IDs: translate those before they are shown.
                if let Some(c) = state.collector.as_ref() {
                    c.send(Item::Upcoming(vec![id]));
                }
                if translate && runtime::get(Flag::LiveTranslate) {
                    crate::live::request(&original, Some(id));
                }
                if let Some(c) = state.collector.as_ref().filter(|_| runtime::get(Flag::Autocollect)) {
                    c.send(Item::Discovered(original));
                }
            }
            ret
        }
    };
    if n.is_multiple_of(5000) {
        log_totals();
    }
    out
}

/// Writes `text` into the game's array, with a trailing `NUL`; returns the new pointer.
/// Like `F_materialize` itself, the count includes the `NUL`.
unsafe fn write_text(t: &Text, arr: *mut u8, text: &str) -> Option<*mut u16> {
    let reserve: ReserveFn = unsafe { std::mem::transmute(t.reserve) };
    let units: Vec<u16> = text.encode_utf16().collect();
    let len = u32::try_from(units.len()).ok()?;
    unsafe {
        reserve(arr, len + 1);
        let ptr = arr.add(t.ptr).cast::<*mut u16>().read();
        let cap = arr.add(t.cap).cast::<u32>().read();
        if ptr.is_null() || cap < len + 1 {
            return None;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
        ptr.add(units.len()).write(0);
        arr.add(t.count).cast::<u32>().write(len + 1);
        Some(ptr)
    }
}

/// Text the game has loaded but not shown yet: if there is no translation,
/// translate it locally before it appears (speech bubble and chat don't ask again).
pub(crate) fn prefetch_if_missing(text: &str, id: Option<u32>) {
    let Some(st) = crate::STATE.get() else { return };
    if missing(st, text) {
        crate::live::prefetch(text, id);
    }
}

/// No translation yet and local translation is on (English client only).
fn missing(st: &crate::State, text: &str) -> bool {
    runtime::get(Flag::Translate)
        && runtime::get(Flag::LiveTranslate)
        && english_client()
        && st.dict.get(text).is_none()
        && crate::live::get(text).is_none()
}

/// A key just arrived for a text we already know: it is about to be shown,
/// so it goes before the ahead-of-time queue.
pub(crate) fn soon_if_missing(text: &str, id: u32) {
    let Some(st) = crate::STATE.get() else { return };
    if missing(st, text) {
        crate::live::soon(text, Some(id));
    }
}

/// Text decrypted for a line already on screen (a speech bubble waiting for
/// it): remembered for [`lookup_id`] and translated before anything else.
pub(crate) fn on_screen_if_missing(text: &str, id: u32) {
    remember_english(id, text);
    let Some(st) = crate::STATE.get() else { return };
    if missing(st, text) {
        crate::live::request(text, Some(id));
    }
}

/// The game decoded `text` recently: it may be on screen or in the chat.
pub(crate) fn decoded_recently(text: &str) -> bool {
    RECENT
        .lock()
        .is_ok_and(|m| m.as_ref().is_some_and(|m| m.values().any(|e| e == text)))
}

/// English of the IDs decoded recently (for [`lookup_id`]): speech bubbles
/// only know the ID of their line.
static RECENT: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);
/// Entries kept at most in [`RECENT`].
const MAX_RECENT: usize = 8192;

fn remember_english(id: u32, english: &str) {
    if let Ok(mut m) = RECENT.lock() {
        let m = m.get_or_insert_with(HashMap::new);
        if m.len() >= MAX_RECENT {
            m.clear();
        }
        if m.get(&id).is_none_or(|e| e != english) {
            m.insert(id, english.to_owned());
        }
    }
}

/// English last decoded for the string `id`.
pub(crate) fn english_of(id: u32) -> Option<String> {
    RECENT.lock().ok()?.as_ref()?.get(&id).cloned()
}

/// The string `id` is in the shipped dictionary (by ID or by its English):
/// the game already shows it translated when it decodes it.
pub(crate) fn in_dictionary(id: u32) -> bool {
    let Some(st) = crate::STATE.get() else { return false };
    st.dict.get_id(id).is_some() || english_of(id).is_some_and(|en| st.dict.get(&en).is_some())
}

/// Translation of the string `id`: by ID in the dictionary, or through the
/// English last decoded for it.
pub(crate) fn lookup_id(id: u32) -> Option<String> {
    let st = crate::STATE.get()?;
    if !runtime::get(Flag::Translate) {
        return None;
    }
    if let Some(tr) = st.dict.get_id(id) {
        return Some(tr.to_owned());
    }
    lookup(&english_of(id)?)
}

/// Translation of `text` (dictionary, then this session's live translations),
/// when translating is on and the translation is usable.
pub(crate) fn lookup(text: &str) -> Option<String> {
    let st = crate::STATE.get()?;
    if !runtime::get(Flag::Translate) {
        return None;
    }
    let tr = st
        .dict
        .get(text)
        .map(str::to_owned)
        .or_else(|| crate::live::get(text))?;
    (!crate::dict::unusable(text, &tr)).then_some(tr)
}

/// For the overlay: strings decoded and strings translated so far.
pub fn stats() -> (u64, u64) {
    (SEEN.load(Ordering::Relaxed), TRANSLATED.load(Ordering::Relaxed))
}

fn collector() -> Option<&'static crate::collect::Collector> {
    let st = crate::STATE.get()?;
    st.collector.as_ref().filter(|_| runtime::get(Flag::SeedDecode))
}

/// Collection and live translation only make sense with the client in English: the
/// text arrives in the client's language, and the files store the English source.
pub(crate) fn english_client() -> bool {
    crate::refresh::current_lang().is_none_or(|l| l == 0)
}

unsafe extern "system" fn reg_insert_hook(cache: *mut u8, lang: u32, id: u32, entry: *const u8) -> u64 {
    if !entry.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe { on_reg_insert(lang, id, entry) });
    }
    unsafe { REG_DETOUR.get().unwrap_unchecked().call(cache, lang, id, entry) }
}

unsafe fn on_reg_insert(lang: u32, id: u32, entry: *const u8) {
    let Some(c) = collector() else { return };
    if lang != 0 || !english_client() {
        return;
    }
    let size = unsafe { entry.cast::<u16>().read_unaligned() } as usize;
    if size <= rc4::HEADER {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(entry, size) }.to_vec();
    let Some(h) = rc4::Header::parse(&bytes) else { return };
    if REG_SAMPLES.fetch_add(1, Ordering::Relaxed) < 5 {
        log!(
            "reg-insert: lang={lang} id={id} size={size} protected={}",
            h.is_protected()
        );
    }
    if h.is_protected() {
        c.send(Item::Entry(id, bytes));
    } else if let Some(text) = rc4::decode(&bytes, None) {
        prefetch_if_missing(&text, Some(id));
        c.send(Item::Discovered(text));
    }
}

unsafe extern "system" fn bulk_insert_hook(rcx: *mut u8, msg: *const u8) -> u32 {
    if !msg.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe { on_bulk_insert(msg) });
    }
    unsafe { BULK_DETOUR.get().unwrap_unchecked().call(rcx, msg) }
}

unsafe fn on_bulk_insert(msg: *const u8) {
    let (Some(c), Some(b)) = (collector(), BULK.get()) else {
        return;
    };
    let count = unsafe { msg.add(b.count).read() } as usize;
    let recs = unsafe { msg.add(b.records).cast::<*const u8>().read_unaligned() };
    if count == 0 || recs.is_null() {
        return;
    }
    let pairs: Vec<(u32, u64)> = (0..count)
        .map(|i| unsafe {
            let r = recs.add(i * layout::BULK_STRIDE);
            (
                r.cast::<u32>().read_unaligned(),
                r.add(layout::BULK_SEED).cast::<u64>().read_unaligned(),
            )
        })
        .collect();
    if BULK_SAMPLES.fetch_add(1, Ordering::Relaxed) < 3 {
        log!(
            "bulk_insert: {count} key(s), first id={} seed={:#x}",
            pairs[0].0,
            pairs[0].1
        );
    }
    c.send(Item::Keys(pairs));
}

pub fn log_totals() {
    log!(
        "text: {} decoded, {} translated ({} by ID), {} encrypted (RC4)",
        SEEN.load(Ordering::Relaxed),
        TRANSLATED.load(Ordering::Relaxed),
        BY_ID.load(Ordering::Relaxed),
        CODED.load(Ordering::Relaxed)
    );
}

/// Creates and enables a detour, storing it in `cell` before enabling it
/// (the hook uses `cell` to call the original).
pub(crate) fn attach<T: Function>(
    img: &Image,
    name: &str,
    addr: usize,
    hook: T,
    cell: &'static OnceLock<GenericDetour<T>>,
) -> bool {
    let target: T = unsafe { T::from_ptr(addr as *const ()) };
    let d = match unsafe { GenericDetour::<T>::new(target, hook) } {
        Ok(d) => cell.get_or_init(|| d),
        Err(e) => {
            log!("hook {name}: failed to create: {e}");
            return false;
        }
    };
    match unsafe { d.enable() } {
        Ok(()) => {
            log!("hook {name} ACTIVE @ rva {:#x}", img.rva(addr));
            true
        }
        Err(e) => {
            log!("hook {name}: failed to enable: {e}");
            false
        }
    }
}

pub fn install(img: &Image, seed_decode: bool) {
    match img.find(&MATERIALIZE).map(|a| (a, layout::text(a))) {
        None => log!(
            "hook {}: signature NOT found — translation disabled for this game version",
            MATERIALIZE.name
        ),
        Some((_, Err(why))) => log!(
            "hook {}: layout changed ({why}) — translation disabled",
            MATERIALIZE.name
        ),
        Some((addr, Ok(t))) => {
            log!("text: {t:x?} (Reserve @ rva {:#x})", img.rva(t.reserve));
            let _ = TEXT.set(t);
            attach(img, MATERIALIZE.name, addr, materialize_hook as MaterializeFn, &DETOUR);
        }
    }
    if !seed_decode {
        return;
    }
    match img.find(&sig::REG_INSERT) {
        Some(a) => {
            attach(
                img,
                sig::REG_INSERT.name,
                a,
                reg_insert_hook as RegInsertFn,
                &REG_DETOUR,
            );
        }
        None => log!(
            "hook {}: signature NOT found — seed_decode partial",
            sig::REG_INSERT.name
        ),
    }
    match img.find(&sig::BULK_INSERT).map(|a| (a, layout::bulk(a))) {
        None => log!(
            "hook {}: signature NOT found — seed_decode partial",
            sig::BULK_INSERT.name
        ),
        Some((_, Err(why))) => log!(
            "hook {}: layout changed ({why}) — seed_decode partial",
            sig::BULK_INSERT.name
        ),
        Some((a, Ok(b))) => {
            let _ = BULK.set(b);
            attach(
                img,
                sig::BULK_INSERT.name,
                a,
                bulk_insert_hook as BulkInsertFn,
                &BULK_DETOUR,
            );
        }
    }
}
