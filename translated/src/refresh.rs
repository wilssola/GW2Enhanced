//! Live refresh: makes the game decode the already displayed text again.
//!
//! `cachetick(cache, ms)` (RVA `0x10dd520`) runs every frame over the text
//! cache and discards, with `evict(node, 1)`, the nodes unused for more than 60 s.
//! The refresh does the same without looking at the age and then calls
//! `frame_rebuild(lang, 0)` (RVA `0x1073200`, message `0x47`), which
//! rebuilds the interface. Everything happens inside the `cachetick` hook, on the
//! game thread. It fires on its own after loading and via the `refresh_key` key.
//!
//! Field and global offsets come from [`layout`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use retour::GenericDetour;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::game::Image;
use crate::layout::{self, Cache, Rebuild};
use crate::{hooks, log, sig};

type CacheTickFn = unsafe extern "system" fn(*mut u8, u32) -> u64;
type EvictFn = unsafe extern "system" fn(*mut u8, u32);
type FrameRebuildFn = unsafe extern "system" fn(u32, u32);

/// Automatic refreshes after the hooks are installed (login, character select, world).
const AUTO: &[Duration] = &[Duration::from_secs(8), Duration::from_secs(30), Duration::from_secs(90)];

/// Minimum interval between refreshes requested by translations of text that
/// is on screen. A refresh evicts the text cache and rebuilds the UI; measured
/// at 35–75 ms (one or two frames), so 3 s keeps dialog windows responsive
/// without noticeable stutter.
const LIVE_INTERVAL: Duration = Duration::from_secs(3);

struct Layout {
    cache: Cache,
    rebuild: Rebuild,
    rebuild_fn: usize,
}

static DETOUR: OnceLock<GenericDetour<CacheTickFn>> = OnceLock::new();
static LAYOUT: OnceLock<Layout> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static NEXT_AUTO: AtomicUsize = AtomicUsize::new(0);
static PENDING: AtomicBool = AtomicBool::new(false);
/// Refresh requested from the overlay: runs on the next frame.
static NOW: AtomicBool = AtomicBool::new(false);
static KEY_DOWN: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);

/// Number of refreshes so far (each one evicts the decoded texts).
pub fn count() -> u64 {
    COUNT.load(Ordering::Relaxed)
}
/// Milliseconds since `START` at the last refresh.
static LAST_MS: AtomicU64 = AtomicU64::new(0);

/// The client's current language (0 = English), when the refresh is active.
pub fn current_lang() -> Option<u32> {
    LAYOUT.get().map(|l| unsafe { (l.rebuild.lang as *const u32).read() })
}

/// Requests a refresh for new translations; several requests in a row become a
/// single one, at most once every [`LIVE_INTERVAL`].
/// Refreshes on the next frame, ignoring the throttle (overlay button).
pub fn request_now() {
    NOW.store(true, Ordering::Relaxed);
}

pub fn request() {
    PENDING.store(true, Ordering::Relaxed);
}

fn game_has_focus() -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == unsafe { GetCurrentProcessId() }
}

unsafe extern "system" fn cachetick_hook(cache: *mut u8, ms: u32) -> u64 {
    let ret = unsafe { DETOUR.get().unwrap_unchecked().call(cache, ms) };
    if !cache.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe { on_tick(cache) });
    }
    ret
}

unsafe fn on_tick(cache: *mut u8) {
    let Some(state) = crate::STATE.get() else { return };
    crate::chat::on_tick();
    if !state.config.live_refresh {
        return;
    }
    let mut why = None;
    let i = NEXT_AUTO.load(Ordering::Relaxed);
    if let Some(&at) = AUTO.get(i)
        && START.get().is_some_and(|s| s.elapsed() >= at)
    {
        NEXT_AUTO.store(i + 1, Ordering::Relaxed);
        why = Some("automatic");
    }
    let down = unsafe { GetAsyncKeyState(state.config.refresh_key as i32) } as u16 & 0x8000 != 0;
    if down && !KEY_DOWN.swap(true, Ordering::Relaxed) && game_has_focus() {
        why = Some("key");
    } else if !down {
        KEY_DOWN.store(false, Ordering::Relaxed);
    }
    if NOW.swap(false, Ordering::Relaxed) {
        why = why.or(Some("overlay"));
    }
    let since_last = START
        .get()
        .map_or(0, |s| s.elapsed().as_millis() as u64)
        .saturating_sub(LAST_MS.load(Ordering::Relaxed));
    if why.is_none() && since_last >= LIVE_INTERVAL.as_millis() as u64 && PENDING.swap(false, Ordering::Relaxed) {
        why = Some("new translations");
    }
    if let Some(why) = why {
        unsafe { refresh(cache, why) };
    }
}

unsafe fn refresh(cache: *mut u8, why: &str) {
    let started = Instant::now();
    let Some(l) = LAYOUT.get() else { return };
    if unsafe { (l.rebuild.ready as *const u8).read() } & 1 == 0 {
        log!("refresh ({why}) postponed: game interface is not ready yet");
        PENDING.store(true, Ordering::Relaxed);
        return;
    }
    let evict: EvictFn = unsafe { std::mem::transmute(l.cache.evict) };
    let rebuild: FrameRebuildFn = unsafe { std::mem::transmute(l.rebuild_fn) };
    let c = &l.cache;
    let mut evicted = 0;
    unsafe {
        let link = cache.add(c.link).cast::<i32>().read() as isize;
        let next_of = |n: *mut u8| {
            let v = n.offset(link + 8).cast::<usize>().read();
            if v & 1 != 0 { std::ptr::null_mut() } else { v as *mut u8 }
        };
        let head = cache.add(c.head).cast::<usize>().read();
        let mut node = if head & 1 != 0 {
            std::ptr::null_mut()
        } else {
            head as *mut u8
        };
        while !node.is_null() {
            let next = next_of(node);
            if node.add(c.size).cast::<u32>().read() != 0 && node.add(c.busy).cast::<u32>().read() == 0 {
                evict(node, 1);
                evicted += 1;
            }
            node = next;
        }
    }
    let lang = unsafe { (l.rebuild.lang as *const u32).read() };
    unsafe { rebuild(lang, 0) };
    LAST_MS.store(
        START.get().map_or(0, |s| s.elapsed().as_millis() as u64),
        Ordering::Relaxed,
    );
    // Chat lines keep a reference to the string: once rebuilt, they come out translated.
    crate::chat::rebuild_all();
    let n = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    log!(
        "refresh #{n} ({why}): {evicted} texts evicted from the cache, frame_rebuild(lang={lang}), {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    hooks::log_totals();
}

pub fn install(img: &Image) {
    let (Some(tick), Some(rebuild_fn)) = (img.find(&sig::CACHETICK), img.find(&sig::FRAME_REBUILD)) else {
        log!("refresh: cachetick/frame_rebuild signatures NOT found — refresh disabled");
        return;
    };
    let (cache, rebuild) = match (layout::cache(tick), layout::rebuild(rebuild_fn)) {
        (Ok(c), Ok(r)) => (c, r),
        (Err(why), _) | (_, Err(why)) => {
            log!("refresh: layout changed ({why}) — refresh disabled");
            return;
        }
    };
    log!(
        "refresh: evict @ rva {:#x}, frame_rebuild @ rva {:#x}, lang @ rva {:#x}, ready @ rva {:#x}, cache {cache:x?}",
        img.rva(cache.evict),
        img.rva(rebuild_fn),
        img.rva(rebuild.lang),
        img.rva(rebuild.ready)
    );
    let _ = LAYOUT.set(Layout {
        cache,
        rebuild,
        rebuild_fn,
    });
    START.get_or_init(Instant::now);
    hooks::attach(img, sig::CACHETICK.name, tick, cachetick_hook as CacheTickFn, &DETOUR);
}
