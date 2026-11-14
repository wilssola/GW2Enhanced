//! Translation of the game's web pages (Trading Post, Gem Store, Wizard's Vault).
//!
//! These pages run in an embedded Chromium (CEF), and their text comes from
//! ArenaNet's servers in string bundles (`strings-en.*.js`), so it never goes
//! through the game's text system. The game's process is CEF's browser
//! process: it creates each page with `cef_browser_host_create_browser`. The DLL
//! hooks that call, wraps the client's load handler and, when a page finishes
//! loading, runs `web_inject.js` in it with `web.jsonl` and `web_local.jsonl`
//! (a few dozen KB).
//!
//! Whatever else the page shows, the script reports through the page console
//! (the DLL wraps the client's display handler to read it). The DLL answers at
//! once from the game dictionary; what is not there goes to the local
//! translator, whose result is written to `web_local.jsonl` (never uploaded)
//! and sent back to the page. The dictionary itself never goes into a page:
//! at ~20 MB of script it held the page up for seconds. A page that changes
//! needs no new release of the mod.
//!
//! Struct offsets come from CEF's C headers for the version the game ships
//! (branch 5060). A different CEF version turns this feature off.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use retour::GenericDetour;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};

use crate::dict::Dict;
use crate::{live, log, records, runtime};

const SCRIPT: &str = include_str!("web_inject.js");
/// The CEF major version these offsets belong to.
const CEF_MAJOR: i32 = 103;
const LOAD_POLL: Duration = Duration::from_millis(25);
/// Prefix of the console messages with the texts a page is missing (`web_inject.js`).
const MARK: &str = "\u{1}gw2t-missing:";
// Limits on what a page may ask the translator for. Any page in the game's browser can
// write to the console, so they are enforced here, not trusted to the script.
/// Longest console message read (UTF-16 units); longer ones are dropped.
const MAX_MESSAGE_UNITS: usize = 256 * 1024;
/// Texts taken from one message.
const MAX_TEXTS_PER_MESSAGE: usize = 200;
/// Longest text sent to the translator.
const MAX_TEXT_CHARS: usize = 2000;
/// Most texts in `web_local.jsonl` plus those requested this session.
const MAX_WEB_TEXTS: usize = 100_000;

// `size` (first field of every CEF struct) and slot offsets, from the C headers.
const REF_RELEASE: usize = 16;
const CLIENT_SIZE: usize = 184;
const CLIENT_GET_DISPLAY_HANDLER: usize = 72;
const CLIENT_GET_LOAD_HANDLER: usize = 144;
const DISPLAY_HANDLER_SIZE: usize = 120;
const DISPLAY_HANDLER_ON_CONSOLE_MESSAGE: usize = 88;
const BROWSER_SIZE: usize = 208;
const BROWSER_IS_VALID: usize = 40;
const BROWSER_GET_IDENTIFIER: usize = 120;
const BROWSER_GET_MAIN_FRAME: usize = 152;
const LOAD_HANDLER_SIZE: usize = 72;
const LOAD_HANDLER_ON_LOAD_END: usize = 56;
const FRAME_SIZE: usize = 240;
const FRAME_EXECUTE_JS: usize = 144;
const FRAME_IS_MAIN: usize = 152;

type VersionInfoFn = unsafe extern "C" fn(i32) -> i32;
type CreateBrowserFn =
    unsafe extern "C" fn(*const c_void, *mut u8, *const c_void, *const c_void, *mut c_void, *mut c_void) -> i32;
type GetHandlerFn = unsafe extern "system" fn(*mut u8) -> *mut u8;
type OnConsoleMessageFn =
    unsafe extern "system" fn(*mut u8, *mut u8, i32, *const CefString, *const CefString, i32) -> i32;
type OnLoadEndFn = unsafe extern "system" fn(*mut u8, *mut u8, *mut u8, i32);
type RefFn = unsafe extern "system" fn(*mut u8) -> i32;
type GetFrameFn = unsafe extern "system" fn(*mut u8) -> *mut u8;
type IsMainFn = unsafe extern "system" fn(*mut u8) -> i32;
type ExecuteJsFn = unsafe extern "system" fn(*mut u8, *const CefString, *const CefString, i32);

/// `cef_string_utf16_t`.
#[repr(C)]
struct CefString {
    text: *const u16,
    length: usize,
    dtor: usize,
}

static CREATE: OnceLock<GenericDetour<CreateBrowserFn>> = OnceLock::new();
/// The script call with the page texts, ready to run.
static PAYLOAD: OnceLock<Vec<u16>> = OnceLock::new();
static DICT: OnceLock<&'static Dict> = OnceLock::new();
/// Original `get_load_handler` of each client, by client address.
static CLIENTS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
/// Original `on_load_end` of each load handler, by handler address.
static HANDLERS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
/// Load handler for clients without one (no reference counting: never freed).
static OWN_HANDLER: OnceLock<usize> = OnceLock::new();
/// Original `get_display_handler` of each client, by client address.
static DISPLAY_CLIENTS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
/// Original `on_console_message` of each display handler, by handler address.
static DISPLAY_HANDLERS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
static OWN_DISPLAY_HANDLER: OnceLock<usize> = OnceLock::new();
/// Browsers that reported missing texts (one reference each), by identifier:
/// their translations are sent to them.
static BROWSERS: Mutex<Vec<(i32, usize)>> = Mutex::new(Vec::new());
/// Web page texts translated in this session (for pages loaded again).
static SESSION: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
/// Texts in `web_local.jsonl` plus texts requested this session (see `MAX_WEB_TEXTS`).
static WEB_TEXTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe fn slot(object: *const u8, offset: usize) -> usize {
    unsafe { object.add(offset).cast::<usize>().read() }
}

/// Replaces a function pointer in a CEF struct, which may be in read-only memory.
unsafe fn patch(object: *mut u8, offset: usize, value: usize) -> bool {
    let at = unsafe { object.add(offset) }.cast::<usize>();
    let mut old = 0;
    unsafe {
        if VirtualProtect(at.cast(), size_of::<usize>(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        at.write(value);
        VirtualProtect(at.cast(), size_of::<usize>(), old, &mut old);
    }
    true
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `entries` as a JavaScript expression.
fn entries_literal<K: serde::Serialize + Eq + std::hash::Hash, V: serde::Serialize>(
    entries: &HashMap<K, V>,
) -> String {
    let json = serde_json::to_string(entries).unwrap_or_else(|_| "{}".into());
    let literal = serde_json::to_string(&json).unwrap_or_else(|_| "\"{}\"".into());
    format!("JSON.parse({literal})")
}

/// A call of the script with `entries`; `complete` marks the last chunk, after
/// which the page reports what is still missing.
fn script_call(entries: &HashMap<&str, &str>, complete: bool) -> String {
    format!("{SCRIPT}({}, {complete});", entries_literal(entries))
}

/// Adds translations to a page that already runs the script.
fn add_call<K: serde::Serialize + Eq + std::hash::Hash, V: serde::Serialize>(entries: &HashMap<K, V>) -> String {
    format!("window.__gw2t && window.__gw2t.add({});", entries_literal(entries))
}

/// Whether a translation can be shown in a page: no game markup.
fn fits_a_page(tr: &str) -> bool {
    !tr.contains(['<', '%', '['])
}

/// A translation the game already has for `text`: the dictionary, then this PC's.
fn known_translation(dict: &Dict, text: &str) -> Option<String> {
    dict.get(text)
        .map(str::to_owned)
        .or_else(|| live::get(text))
        .filter(|tr| fits_a_page(tr) && tr != text)
}

/// `web` (written for the pages) wins over `web_local` (translated on this PC).
fn build_payload(web: &[records::Translation], web_local: &[records::Translation]) -> Vec<u16> {
    let texts: HashMap<&str, &str> = web_local
        .iter()
        .chain(web)
        .map(|t| (t.en.as_str(), t.tr.as_str()))
        .collect();
    utf16(&script_call(&texts, true))
}

unsafe fn execute(frame: *mut u8, code: &[u16]) {
    let run: ExecuteJsFn = unsafe { std::mem::transmute(slot(frame, FRAME_EXECUTE_JS)) };
    let code = CefString {
        text: code.as_ptr(),
        length: code.len(),
        dtor: 0,
    };
    let url = CefString {
        text: std::ptr::null(),
        length: 0,
        dtor: 0,
    };
    unsafe { run(frame, &code, &url, 0) };
}

/// Runs the script in a page that finished loading.
unsafe fn inject(frame: *mut u8) {
    if frame.is_null() || !enabled() {
        return;
    }
    let Some(payload) = PAYLOAD.get() else { return };
    if unsafe { slot(frame, 0) } != FRAME_SIZE {
        log!("web: unexpected cef_frame_t size; page not translated");
        return;
    }
    let is_main: IsMainFn = unsafe { std::mem::transmute(slot(frame, FRAME_IS_MAIN)) };
    if unsafe { is_main(frame) } == 0 {
        return;
    }
    unsafe { execute(frame, payload) };
    let session = SESSION.lock().ok().and_then(|s| s.as_ref().map(add_call));
    if let Some(session) = session {
        unsafe { execute(frame, &utf16(&session)) };
    }
}

unsafe fn release(object: *mut u8) {
    if !object.is_null() {
        let release: RefFn = unsafe { std::mem::transmute(slot(object, REF_RELEASE)) };
        unsafe { release(object) };
    }
}

fn enabled() -> bool {
    runtime::get(runtime::Flag::Translate) && runtime::get(runtime::Flag::WebTranslate)
}

/// The string, or `None` when it is longer than `MAX_MESSAGE_UNITS`.
unsafe fn read_string(s: *const CefString) -> Option<String> {
    if s.is_null() {
        return Some(String::new());
    }
    let s = unsafe { &*s };
    if s.text.is_null() || s.length == 0 {
        return Some(String::new());
    }
    let units = unsafe { std::slice::from_raw_parts(s.text, s.length.min(MAX_MESSAGE_UNITS + 1)) };
    if s.length > MAX_MESSAGE_UNITS {
        // Still ours to hide when it carries the mark.
        let mark: Vec<u16> = MARK.encode_utf16().collect();
        return units.starts_with(&mark).then(|| MARK.to_owned());
    }
    Some(String::from_utf16_lossy(units))
}

/// The texts in a message from `web_inject.js`; `None` for any other message.
fn missing_texts(message: &str) -> Option<Vec<String>> {
    let list = message.strip_prefix(MARK)?;
    let texts: Vec<String> = serde_json::from_str(list).unwrap_or_default();
    Some(
        texts
            .into_iter()
            .filter(|t| t.chars().count() <= MAX_TEXT_CHARS && t.chars().any(char::is_alphabetic))
            .take(MAX_TEXTS_PER_MESSAGE)
            .collect(),
    )
}

/// Takes room for `wanted` more texts under `MAX_WEB_TEXTS`; returns how many fit.
fn take_room(wanted: usize) -> usize {
    use std::sync::atomic::Ordering;
    let mut granted = 0;
    let _ = WEB_TEXTS.try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
        granted = wanted.min(MAX_WEB_TEXTS.saturating_sub(used));
        Some(used + granted)
    });
    if granted < wanted {
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            log!("web: limit of {MAX_WEB_TEXTS} page texts reached; the rest stays untranslated");
        }
    }
    granted
}

/// Keeps a reference to `browser` (owned by the caller of this) to send it translations.
unsafe fn remember(browser: *mut u8) {
    if browser.is_null() {
        return;
    }
    if unsafe { slot(browser, 0) } != BROWSER_SIZE {
        log!("web: unexpected cef_browser_t size; page texts not sent back");
        unsafe { release(browser) };
        return;
    }
    let get_id: RefFn = unsafe { std::mem::transmute(slot(browser, BROWSER_GET_IDENTIFIER)) };
    let id = unsafe { get_id(browser) };
    let Ok(mut browsers) = BROWSERS.lock() else {
        unsafe { release(browser) };
        return;
    };
    if browsers.iter().any(|&(known, _)| known == id) {
        unsafe { release(browser) };
    } else {
        browsers.push((id, browser as usize));
    }
}

/// Runs `code` in the main frame of `browser`.
unsafe fn run_in(browser: *mut u8, code: &[u16]) {
    let main_frame: GetFrameFn = unsafe { std::mem::transmute(slot(browser, BROWSER_GET_MAIN_FRAME)) };
    let frame = unsafe { main_frame(browser) };
    if !frame.is_null() {
        unsafe {
            execute(frame, code);
            release(frame);
        }
    }
}

/// Sends translations made by the local translator to the pages that asked for them.
pub fn deliver(pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    let new: HashMap<&str, &str> = pairs.iter().map(|(en, tr)| (en.as_str(), tr.as_str())).collect();
    if let Ok(mut session) = SESSION.lock() {
        session
            .get_or_insert_default()
            .extend(pairs.iter().map(|(en, tr)| (en.clone(), tr.clone())));
    }
    if !enabled() {
        return;
    }
    let code = utf16(&add_call(&new));
    let Ok(mut browsers) = BROWSERS.lock() else { return };
    browsers.retain(|&(_, browser)| {
        let browser = browser as *mut u8;
        let is_valid: IsMainFn = unsafe { std::mem::transmute(slot(browser, BROWSER_IS_VALID)) };
        if unsafe { is_valid(browser) } == 0 {
            unsafe { release(browser) };
            return false;
        }
        unsafe { run_in(browser, &code) };
        true
    });
}

unsafe extern "system" fn on_console_message(
    handler: *mut u8,
    browser: *mut u8,
    level: i32,
    message: *const CefString,
    source: *const CefString,
    line: i32,
) -> i32 {
    let ours = std::panic::catch_unwind(|| {
        let texts = unsafe { read_string(message) }.and_then(|m| missing_texts(&m))?;
        if enabled() && !browser.is_null() && unsafe { slot(browser, 0) } == BROWSER_SIZE {
            let mut found = HashMap::new();
            let mut missing = Vec::new();
            for text in texts {
                match DICT.get().and_then(|d| known_translation(d, &text)) {
                    Some(tr) => {
                        found.insert(text, tr);
                    }
                    None => missing.push(text),
                }
            }
            if !found.is_empty() {
                unsafe { run_in(browser, &utf16(&add_call(&found))) };
            }
            let room = take_room(missing.len());
            for text in missing.iter().take(room) {
                live::request_web(text);
            }
            unsafe { remember(browser) };
        } else {
            unsafe { release(browser) };
        }
        Some(())
    });
    if matches!(ours, Ok(Some(()))) {
        // Hidden from the console.
        return 1;
    }
    let original = DISPLAY_HANDLERS
        .lock()
        .ok()
        .and_then(|h| h.as_ref()?.get(&(handler as usize)).copied());
    match original {
        Some(f) => unsafe {
            let f: OnConsoleMessageFn = std::mem::transmute(f);
            f(handler, browser, level, message, source, line)
        },
        None => {
            unsafe { release(browser) };
            0
        }
    }
}

unsafe extern "system" fn on_load_end(handler: *mut u8, browser: *mut u8, frame: *mut u8, status: i32) {
    let _ = std::panic::catch_unwind(|| unsafe { inject(frame) });
    let original = HANDLERS
        .lock()
        .ok()
        .and_then(|h| h.as_ref()?.get(&(handler as usize)).copied());
    match original {
        // The callee owns the references it receives.
        Some(f) => unsafe {
            let f: OnLoadEndFn = std::mem::transmute(f);
            f(handler, browser, frame, status);
        },
        None => unsafe {
            release(browser);
            release(frame);
        },
    }
}

unsafe extern "system" fn no_ref(_: *mut u8) {}
unsafe extern "system" fn release_ref(_: *mut u8) -> i32 {
    0
}
unsafe extern "system" fn has_ref(_: *mut u8) -> i32 {
    1
}

/// A handler struct of `size` bytes owned by the DLL, with `callback` at `offset`.
fn static_handler(size: usize, offset: usize, callback: usize) -> usize {
    let mut words = vec![0usize; size / size_of::<usize>()].into_boxed_slice();
    words[0] = size;
    words[1] = no_ref as unsafe extern "system" fn(*mut u8) as usize;
    words[2] = release_ref as RefFn as usize;
    words[3] = has_ref as RefFn as usize;
    words[4] = has_ref as RefFn as usize;
    words[offset / size_of::<usize>()] = callback;
    Box::leak(words).as_ptr() as usize
}

fn own_display_handler() -> *mut u8 {
    *OWN_DISPLAY_HANDLER.get_or_init(|| {
        static_handler(
            DISPLAY_HANDLER_SIZE,
            DISPLAY_HANDLER_ON_CONSOLE_MESSAGE,
            on_console_message as OnConsoleMessageFn as usize,
        )
    }) as *mut u8
}

/// Wraps `on_console_message` in the display handler the game's client returned.
unsafe extern "system" fn get_display_handler(client: *mut u8) -> *mut u8 {
    let original = DISPLAY_CLIENTS
        .lock()
        .ok()
        .and_then(|c| c.as_ref()?.get(&(client as usize)).copied());
    let handler = match original {
        Some(f) if f != 0 => unsafe {
            let f: GetHandlerFn = std::mem::transmute(f);
            f(client)
        },
        _ => std::ptr::null_mut(),
    };
    if handler.is_null() {
        return own_display_handler();
    }
    if unsafe { slot(handler, 0) } != DISPLAY_HANDLER_SIZE {
        log!("web: unexpected cef_display_handler_t size; page texts not translated on this PC");
        return handler;
    }
    let ours = on_console_message as OnConsoleMessageFn as usize;
    let current = unsafe { slot(handler, DISPLAY_HANDLER_ON_CONSOLE_MESSAGE) };
    if current != ours {
        if let Ok(mut handlers) = DISPLAY_HANDLERS.lock() {
            handlers.get_or_insert_default().insert(handler as usize, current);
        }
        unsafe { patch(handler, DISPLAY_HANDLER_ON_CONSOLE_MESSAGE, ours) };
    }
    handler
}

fn own_handler() -> *mut u8 {
    *OWN_HANDLER.get_or_init(|| {
        static_handler(
            LOAD_HANDLER_SIZE,
            LOAD_HANDLER_ON_LOAD_END,
            on_load_end as OnLoadEndFn as usize,
        )
    }) as *mut u8
}

/// Wraps `on_load_end` in the load handler the game's client returned.
unsafe extern "system" fn get_load_handler(client: *mut u8) -> *mut u8 {
    let original = CLIENTS
        .lock()
        .ok()
        .and_then(|c| c.as_ref()?.get(&(client as usize)).copied());
    let handler = match original {
        Some(f) => unsafe {
            let f: GetHandlerFn = std::mem::transmute(f);
            f(client)
        },
        None => std::ptr::null_mut(),
    };
    if handler.is_null() {
        return own_handler();
    }
    if unsafe { slot(handler, 0) } != LOAD_HANDLER_SIZE {
        log!("web: unexpected cef_load_handler_t size; pages not translated");
        return handler;
    }
    if unsafe { slot(handler, LOAD_HANDLER_ON_LOAD_END) } != on_load_end as OnLoadEndFn as usize {
        let current = unsafe { slot(handler, LOAD_HANDLER_ON_LOAD_END) };
        if let Ok(mut handlers) = HANDLERS.lock() {
            handlers.get_or_insert_default().insert(handler as usize, current);
        }
        unsafe { patch(handler, LOAD_HANDLER_ON_LOAD_END, on_load_end as OnLoadEndFn as usize) };
    }
    handler
}

unsafe extern "C" fn create_browser(
    info: *const c_void,
    client: *mut u8,
    url: *const c_void,
    settings: *const c_void,
    extra: *mut c_void,
    context: *mut c_void,
) -> i32 {
    let _ = std::panic::catch_unwind(|| {
        if client.is_null() {
            return;
        }
        if unsafe { slot(client, 0) } != CLIENT_SIZE {
            log!("web: unexpected cef_client_t size; pages not translated");
            return;
        }
        let current = unsafe { slot(client, CLIENT_GET_LOAD_HANDLER) };
        if current == get_load_handler as GetHandlerFn as usize {
            return;
        }
        let display = unsafe { slot(client, CLIENT_GET_DISPLAY_HANDLER) };
        if let Ok(mut clients) = DISPLAY_CLIENTS.lock() {
            clients.get_or_insert_default().insert(client as usize, display);
        }
        unsafe {
            patch(
                client,
                CLIENT_GET_DISPLAY_HANDLER,
                get_display_handler as GetHandlerFn as usize,
            )
        };
        if let Ok(mut clients) = CLIENTS.lock() {
            clients.get_or_insert_default().insert(client as usize, current);
        }
        if unsafe {
            patch(
                client,
                CLIENT_GET_LOAD_HANDLER,
                get_load_handler as GetHandlerFn as usize,
            )
        } {
            log!("web: page client wrapped");
        }
    });
    unsafe {
        CREATE
            .get()
            .unwrap_unchecked()
            .call(info, client, url, settings, extra, context)
    }
}

/// Hooks CEF as soon as the game loads it (it is delay-loaded).
fn hook_cef() {
    let name = utf16("libcef.dll\0");
    let module = loop {
        let m = unsafe { GetModuleHandleW(name.as_ptr()) };
        if !m.is_null() {
            break m;
        }
        std::thread::sleep(LOAD_POLL);
    };
    let (Some(version), Some(create)) = (
        unsafe { GetProcAddress(module, c"cef_version_info".as_ptr().cast()) },
        unsafe { GetProcAddress(module, c"cef_browser_host_create_browser".as_ptr().cast()) },
    ) else {
        log!("web: CEF exports not found; pages not translated");
        return;
    };
    let version: VersionInfoFn = unsafe { std::mem::transmute(version) };
    let major = unsafe { version(0) };
    if major != CEF_MAJOR {
        log!("web: CEF {major} instead of {CEF_MAJOR}; pages not translated");
        return;
    }
    let target: CreateBrowserFn = unsafe { std::mem::transmute(create) };
    let hooked = unsafe { GenericDetour::<CreateBrowserFn>::new(target, create_browser) }
        .map(|d| CREATE.get_or_init(|| d))
        .and_then(|d| unsafe { d.enable() });
    match hooked {
        Ok(()) => log!("web: hook cef_browser_host_create_browser ACTIVE"),
        Err(e) => log!("web: hook failed: {e}"),
    }
}

/// Prepares the scripts and waits for CEF, without blocking the game.
pub fn install(web_file: PathBuf, web_local_file: PathBuf, dict: &'static Dict) {
    let _ = std::thread::Builder::new()
        .name("gw2translated-web".into())
        .spawn(move || {
            let web = match records::read::<records::Translation>(&web_file) {
                Ok(r) => r.records,
                Err(e) => {
                    log!("web: {} could not be read: {e}", web_file.display());
                    Vec::new()
                }
            };
            // Missing until the translator makes the first one.
            let web_local = records::read::<records::Translation>(&web_local_file)
                .map(|r| r.records)
                .unwrap_or_default();
            WEB_TEXTS.fetch_add(web_local.len(), std::sync::atomic::Ordering::Relaxed);
            let _ = DICT.set(dict);
            let payload = build_payload(&web, &web_local);
            log!(
                "web: {} page texts, {} translated on this PC, script of {} KB",
                web.len(),
                web_local.len(),
                payload.len() * 2 / 1024
            );
            let _ = PAYLOAD.set(payload);
            hook_cef();
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::Command;

    fn shipped() -> Vec<records::Translation> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/languages/pt-BR/web.jsonl");
        records::read::<records::Translation>(&p).unwrap().records
    }

    fn placeholders(s: &str) -> HashSet<&str> {
        s.split('{').skip(1).filter_map(|p| p.split('}').next()).collect()
    }

    #[test]
    fn shipped_page_texts_keep_their_placeholders() {
        let all = shipped();
        assert!(all.len() > 300);
        let mut seen = HashSet::new();
        for t in &all {
            assert!(seen.insert(t.en.as_str()), "repeated: {}", t.en);
            assert_eq!(placeholders(&t.en), placeholders(&t.tr), "{}", t.en);
        }
    }

    #[test]
    fn pages_get_dictionary_texts_without_markup() {
        let mut dict = Dict::default();
        dict.add(
            [
                ("Mystic Coin", "Moeda Mística"),
                ("Coin", "<c=@task>Moeda</c>"),
                ("Same", "Same"),
            ]
            .map(|(en, tr)| records::Translation {
                en: en.into(),
                tr: tr.into(),
                src: None,
                ids: Vec::new(),
            }),
            true,
        );
        assert_eq!(known_translation(&dict, "Mystic Coin").as_deref(), Some("Moeda Mística"));
        assert_eq!(known_translation(&dict, "Coin"), None);
        assert_eq!(known_translation(&dict, "Same"), None);
        assert_eq!(known_translation(&dict, "Unknown"), None);
    }

    #[test]
    fn script_call_survives_every_character() {
        let entries = HashMap::from([("a \"quoted\" \\ </script>\n", "b \u{2028} ção")]);
        let call = script_call(&entries, true);
        assert!(call.starts_with("// Translates"));
        assert!(call.ends_with("), true);"));
    }

    /// Runs the real script on the strings of the screenshots in Node, when it is installed.
    #[test]
    fn script_translates_the_trading_post() {
        let mut texts: HashMap<&str, &str> = HashMap::new();
        let all = shipped();
        for t in &all {
            texts.insert(&t.en, &t.tr);
        }
        texts.insert("Armor", "Armadura");
        texts.insert("Search", "Pesquisar");
        let check = r#"
            const t = window.__gw2t.translate;
            const eq = (a, b) => { if (a !== b) throw new Error(JSON.stringify([a, b])); };
            eq(t("Armor"), "Armadura");
            eq(t("Jade Bot Components"), "Componentes de Robô de Jade");
            eq(t("Inventory Bags"), "Bolsas de Inventário");
            eq(t("Skins"), "Visuais");
            eq(t("Delivery Box"), "Caixa de Entrega");
            eq(t("Get More Gold"), "Obter Mais Ouro");
            eq(t("  Search\n"), "  Pesquisar\n");
            eq(t("12 days ago"), "há 12 dias");
            eq(t("about 3 hours"), "cerca de 3 horas");
            eq(t("Buying 2/10"), "Comprando 2/10");
            eq(t("Mystic Coin"), null);
            eq(t("1 234"), null);
            eq(t("Armadura"), null);
            window.__gw2t.add({"Mystic Coin": "Moeda Mística"});
            eq(t("Mystic Coin"), "Moeda Mística");
            // Missing texts go to the console once, after the last chunk.
            const sent = [];
            console.debug = (m) => sent.push(m);
            eq(t("Brand new label"), null);
            eq(t("Brand new label"), null);
            window.__gw2t.add({}, true);
            setTimeout(() => {
                eq(sent.length, 1);
                eq(sent[0], "gw2t-missing:" + JSON.stringify(["Brand new label"]));
                window.__gw2t.add({"Brand new label": "Rótulo novo"});
                eq(t("Brand new label"), "Rótulo novo");
            }, 400);
        "#;
        let program = format!("globalThis.window = globalThis;\n{}\n{check}", script_call(&texts, false));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("check.js");
        std::fs::write(&file, program).unwrap();
        let Ok(out) = Command::new("node").arg(&file).output() else {
            return;
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn page_messages_are_limited() {
        assert_eq!(missing_texts("other"), None);
        assert_eq!(missing_texts(&format!("{MARK}not json")), Some(vec![]));
        let many: Vec<String> = (0..500).map(|i| format!("text {i}")).collect();
        let msg = format!("{MARK}{}", serde_json::to_string(&many).unwrap());
        assert_eq!(missing_texts(&msg).unwrap().len(), MAX_TEXTS_PER_MESSAGE);
        let odd = vec!["123".to_owned(), "x".repeat(MAX_TEXT_CHARS + 1), "Ok".to_owned()];
        let msg = format!("{MARK}{}", serde_json::to_string(&odd).unwrap());
        assert_eq!(missing_texts(&msg), Some(vec!["Ok".to_owned()]));
        let long: Vec<u16> = format!("{MARK}{}", "a".repeat(MAX_MESSAGE_UNITS)).encode_utf16().collect();
        let s = CefString { text: long.as_ptr(), length: long.len(), dtor: 0 };
        assert_eq!(unsafe { read_string(&s) }.as_deref(), Some(MARK));
        WEB_TEXTS.store(MAX_WEB_TEXTS - 3, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(take_room(10), 3);
        assert_eq!(take_room(1), 0);
    }

    /// CEF's string, as its C API lays it out.
    #[test]
    fn cef_string_layout() {
        assert_eq!(size_of::<CefString>(), 24);
    }
}
