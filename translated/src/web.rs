//! Translation of the game's web pages (Trading Post, Gem Store, Wizard's Vault).
//!
//! These pages run in an embedded Chromium (CEF), and their text comes from
//! ArenaNet's servers in string bundles (`strings-en.*.js`), so it never goes
//! through the game's text system. The game's process is CEF's browser
//! process: it creates each page with `cef_browser_host_create_browser`. The DLL
//! hooks that call, wraps the client's load handler and, when a page finishes
//! loading, runs `web_inject.js` in it.
//!
//! Whatever is still missing after `web.jsonl` and the dictionary, the script
//! reports through the page console (the DLL wraps the client's display
//! handler to read it): the local translator translates it, the result goes to
//! `web_local.jsonl` (never uploaded) and back to the page. A page that
//! changes needs no new release of the mod.
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
/// Dictionary texts longer than this stay out of the pages (labels and item
/// names are short; the whole dictionary would not be worth the size).
const MAX_DICTIONARY_CHARS: usize = 64;
const LOAD_POLL: Duration = Duration::from_millis(25);
/// Prefix of the console messages with the texts a page is missing (`web_inject.js`).
const MARK: &str = "\u{1}gw2t-missing:";

// `size` (first field of every CEF struct) and slot offsets, from the C headers.
const REF_ADD: usize = 8;
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

/// A CEF object pointer, sent to the thread that delivers the dictionary.
struct Frame(*mut u8);
unsafe impl Send for Frame {}

/// Scripts ready to run in a page.
struct Payload {
    texts: Vec<u16>,
    dictionary: Vec<u16>,
}

static CREATE: OnceLock<GenericDetour<CreateBrowserFn>> = OnceLock::new();
static PAYLOAD: OnceLock<Payload> = OnceLock::new();
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

/// Whether a dictionary entry can be shown in a page: short and with no game markup.
fn fits_a_page(en: &str, tr: &str) -> bool {
    en.chars().count() <= MAX_DICTIONARY_CHARS
        && en.chars().any(char::is_alphabetic)
        && ![en, tr].iter().any(|t| t.contains(['<', '%', '[']))
}

/// `web` (written for the pages) wins over `web_local` (translated on this PC).
fn build_payload(web: &[records::Translation], web_local: &[records::Translation], dict: &Dict) -> Payload {
    let texts: HashMap<&str, &str> = web_local
        .iter()
        .chain(web)
        .map(|t| (t.en.as_str(), t.tr.as_str()))
        .collect();
    let dictionary: HashMap<&str, &str> = dict
        .iter()
        .filter(|(en, tr)| fits_a_page(en, tr) && !texts.contains_key(*en))
        .collect();
    Payload {
        texts: utf16(&script_call(&texts, false)),
        dictionary: utf16(&script_call(&dictionary, true)),
    }
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

/// Runs the scripts in a page that finished loading. The dictionary goes on
/// another thread: this one is the game's own.
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
    unsafe { execute(frame, &payload.texts) };
    let add_ref: RefFn = unsafe { std::mem::transmute(slot(frame, REF_ADD)) };
    unsafe { add_ref(frame) };
    let frame = Frame(frame);
    let _ = std::thread::Builder::new()
        .name("gw2translated-web".into())
        .spawn(move || {
            let frame = frame;
            let session = SESSION.lock().ok().and_then(|s| s.as_ref().map(add_call));
            unsafe {
                if let Some(session) = session {
                    execute(frame.0, &utf16(&session));
                }
                execute(frame.0, &payload.dictionary);
                release(frame.0);
            }
        });
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

unsafe fn read_string(s: *const CefString) -> String {
    if s.is_null() {
        return String::new();
    }
    let s = unsafe { &*s };
    if s.text.is_null() || s.length == 0 {
        return String::new();
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s.text, s.length) })
}

/// The texts in a message from `web_inject.js`; `None` for any other message.
fn missing_texts(message: &str) -> Option<Vec<String>> {
    let list = message.strip_prefix(MARK)?;
    Some(serde_json::from_str(list).unwrap_or_default())
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
        let main_frame: GetFrameFn = unsafe { std::mem::transmute(slot(browser, BROWSER_GET_MAIN_FRAME)) };
        let frame = unsafe { main_frame(browser) };
        if !frame.is_null() {
            unsafe {
                execute(frame, &code);
                release(frame);
            }
        }
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
        let texts = missing_texts(&unsafe { read_string(message) })?;
        if enabled() {
            for text in &texts {
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
            let payload = build_payload(&web, &web_local, dict);
            log!(
                "web: {} page texts, {} translated on this PC, scripts of {} and {} KB",
                web.len(),
                web_local.len(),
                payload.texts.len() / 1024,
                payload.dictionary.len() / 1024
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
    fn only_short_plain_texts_go_to_a_page() {
        assert!(fits_a_page("Mystic Coin", "Moeda Mística"));
        assert!(!fits_a_page("<c=@task>Mystic</c>", "Mística"));
        assert!(!fits_a_page("%str1% Coin", "Moeda %str1%"));
        assert!(!fits_a_page("12345", "12345"));
        assert!(!fits_a_page(&"long ".repeat(20), "x"));
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

    /// CEF's string, as its C API lays it out.
    #[test]
    fn cef_string_layout() {
        assert_eq!(size_of::<CefString>(), 24);
    }
}
