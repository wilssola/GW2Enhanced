//! "Apply now" for the overlay toggle: re-compose every character so the armor/
//! skin/face pieces are read from disk again (through our file hook).
//!
//! The game never rebuilds composites by itself (Texture Detail only reloads
//! device textures). Recomposition is scheduled with the engine's own
//! `CpsComposite::Schedule` (0x12d6880) after marking every component invalid
//! (`+0xae4 |= <all bits>`); flag `0x10000` set by it releases the cached source
//! pieces, which are then re-read via `CreateFromFile`. The lists are not locked,
//! so this must run on the game thread: we hook the per-frame texture advance
//! (call right after the "ViewAdvanceTexture" marker) and do it there.
//! See docs/texture-reverse-engineering.md, "Recarregar texturas em jogo".
//!
//! Also covers NPC/variant composites and model textures, through the engine's own
//! `Reload` as used by Texture Detail. Work is spread over frames, and only ever runs
//! after a click. An NPC appearance is shared: the first composite owns it (flag
//! 0x8000, has the piece data), the others just point at its final textures, so only
//! the owner is rebuilt. An owner destroyed mid-rebuild trips the engine's assert
//! "m_updateState == COMPOSITE_STATE_DONE" in its release; see `release`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use retour::GenericDetour;

use crate::game::Image;
use crate::log;
use crate::sig::{Pattern, Site};

/// `lea rcx,"ViewAdvanceTexture"; call; call <tex advance thunk>; call; lea ...`
const VIEW_ADVANCE: Site = Site {
    name: "view_advance_texture",
    sig: "48 8D 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? E8 ?? ?? ?? ?? E8 ?? ?? ?? ?? 48 8D 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 41 0F",
};
/// Composite global-list link (CpsComposite ctor helper): `lea rcx, s_list` at +0x26.
const LIST_LINK: Site = Site {
    name: "composite_list_link",
    sig: "48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 48 83 EC 20 48 8B DA 49 8B F1 48 8B",
};
/// `CpsComposite::Schedule(c)` — acts on states 1 and 7.
const SCHEDULE: Site = Site { name: "composite_schedule", sig: "40 53 48 83 EC 20 8B 81 44 0B 00 00" };

type Advance = unsafe extern "system" fn(u64, u64, u64, u64) -> u64;
type Schedule = unsafe extern "system" fn(usize) -> u64;
type TexReload = unsafe extern "system" fn(usize, u32);
type Release = unsafe extern "system" fn(usize) -> u64;

/// Composite release (from the dtor; asserts on an owner that is not done).
const RELEASE: Site = Site { name: "composite_release", sig: "48 8B C4 53 56 41 54 41 55 48 83 EC 68 48 8B 99" };
static RELEASE_HOOK: OnceLock<GenericDetour<Release>> = OnceLock::new();
/// Owners released in the middle of a rebuild.
static N_RELEASED_BUSY: AtomicU32 = AtomicU32::new(0);
/// Clicks closer than this are merged (the latest settings apply).
const MIN_REQUEST_GAP_MS: u128 = 1000;
static LAST_REQUEST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
static NOT_BEFORE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
/// A reload is still running (shown in the overlay).
pub static BUSY: AtomicBool = AtomicBool::new(false);
const FLAG_SHARED_OWNER: u32 = 0x8000;
const OFF_FLAGS: usize = 0xb1c;

/// `GrTex2dSetQuality`: `mov rbp,[rip+disp]` (s_fileManager) at +0x47.
const SET_QUALITY: Site = Site { name: "grtex2d_set_quality", sig: "48 83 EC 48 8B D1 85 C9 74 23 83 EA" };
/// `CIGrTexture2dData::Reload(rec, force)`.
const TEX_RELOAD: Site = Site { name: "grtex2d_reload", sig: "40 53 57 41 56 48 83 EC 20 48 83 79" };
static FILE_MGR: OnceLock<usize> = OnceLock::new();
static RELOAD_FN: OnceLock<usize> = OnceLock::new();

/// Config `live_reload_npc` / `live_reload_models` (default on): the two parts of a
/// click that can be turned off separately if one of them misbehaves.
pub static LIVE_NPC: AtomicBool = AtomicBool::new(true);
pub static LIVE_MODELS: AtomicBool = AtomicBool::new(true);
/// A reload happened: a larger HD image over an original record is now expected,
/// so it must not put the file on the "fixed-size record" deny list.
pub static RELOADED: AtomicBool = AtomicBool::new(false);
/// Model textures sent to reload by the last request.
pub static LAST_TEXTURES: AtomicU32 = AtomicU32::new(0);

static TEX_WAITING: AtomicU32 = AtomicU32::new(0);
const STATS_EVERY_SECS: u64 = 30;
static LAST_STATS: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Overlay figure "VRAM mod": what the HD textures still held by the engine add over the originals,
/// plus the larger atlases of the characters in the list. Only while the panel is open, every 2 s.
fn probe_mod_vram() {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if !crate::overlay::is_visible() {
        return;
    }
    {
        let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some_and(|t| t.elapsed().as_secs() < 2) {
            return;
        }
        *g = Some(std::time::Instant::now());
    }
    let Some(&mgr_at) = FILE_MGR.get() else { return };
    let mgr: usize = rd(mgr_at);
    if mgr == 0 {
        return;
    }
    let off = rd::<i32>(mgr) as usize;
    let mut resident = std::collections::HashSet::new();
    let mut p: usize = rd(mgr + 0x10);
    let mut n = 0u32;
    while p != 0 && p & 1 == 0 && n < 2_000_000 {
        n += 1;
        let id: u32 = rd(p + 0x30);
        if id != 0 {
            resident.insert(id);
        }
        p = rd(p + off + 8);
    }
    let textures = crate::diag::vram_extra_resident(&resident);
    // Final textures of a composite: two ~1 byte/pixel textures (+1/3 for mips); vanilla is 1024 wide.
    let mut atlases = 0f64;
    for c in composites() {
        let s = crate::composite::current_scale(c).max(1) as f64;
        atlases += 2.0 * (1024.0 * 1024.0) * (s * s - 1.0) * 4.0 / 3.0;
    }
    crate::mem::set_mod_vram(textures + atlases as u64);
}

/// Periodic log: does the engine really keep one record per texture file?
fn log_stats() {
    {
        let mut g = LAST_STATS.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some_and(|t| t.elapsed().as_secs() < STATS_EVERY_SECS) {
            return;
        }
        *g = Some(std::time::Instant::now());
    }
    let Some(&mgr_at) = FILE_MGR.get() else { return };
    let mgr: usize = rd(mgr_at);
    if mgr == 0 {
        return;
    }
    let off = rd::<i32>(mgr) as usize;
    // file id -> (record, record flags, approx. bytes: 1 byte per pixel + mips)
    let mut by_id: std::collections::HashMap<u32, Vec<(usize, u32, u64)>> = std::collections::HashMap::new();
    let mut files = 0u32;
    let mut p: usize = rd(mgr + 0x10);
    while p != 0 && p & 1 == 0 && files < 2_000_000 {
        files += 1;
        let (id, rec): (u32, usize) = (rd(p + 0x30), rd(p + 0x28));
        if id != 0 && rec != 0 {
            let px = rd::<u32>(rec + 0x48) as u64 * rd::<u32>(rec + 0x4c) as u64;
            let v = by_id.entry(id).or_default();
            if !v.iter().any(|e| e.0 == rec) {
                v.push((rec, rd(rec + 0x60), px * 4 / 3));
            }
        }
        p = rd(p + off + 8);
    }
    let dup: Vec<&Vec<(usize, u32, u64)>> = by_id.values().filter(|v| v.len() > 1).collect();
    let extra: u64 = dup.iter().map(|v| v.iter().map(|e| e.2).sum::<u64>() - v.iter().map(|e| e.2).max().unwrap_or(0)).sum();
    let mut kinds: std::collections::HashMap<Vec<u32>, u32> = std::collections::HashMap::new();
    for v in &dup {
        let mut f: Vec<u32> = v.iter().map(|e| e.1).collect();
        f.sort_unstable();
        *kinds.entry(f).or_default() += 1;
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort_unstable_by_key(|k| std::cmp::Reverse(k.1));
    kinds.truncate(4);
    let all = composites();
    let finals: std::collections::HashSet<usize> = all.iter().map(|&c| rd::<usize>(c + 0xb48)).collect();
    let (cache, cached_files, keep_only) = crate::diag::memory_stats();
    log!(
        "mem: {files} texture files, {} ids, {} ids with several records (~{} MB extra, flags {:x?}); {} composites, {} final textures; RAM cache {} MB/{cached_files} files, ring {} MB, shared builds {}, cache hits {}",
        by_id.len(),
        dup.len(),
        extra >> 20,
        kinds,
        all.len(),
        finals.len(),
        cache >> 20,
        keep_only >> 20,
        crate::diag::N_SHARED_BUILD.load(Ordering::Relaxed),
        crate::diag::N_CACHE_HIT.load(Ordering::Relaxed)
    );
}
const COMPOSITES_PER_FRAME: usize = 16;
const TEXTURES_PER_FRAME: usize = 48;
const TEXTURE_FRAMES: u32 = 36000;
/// Records already reloaded by this request, and the frames left to finish it.
static TEX_DONE: std::sync::Mutex<Option<(std::collections::HashSet<usize>, u32)>> = std::sync::Mutex::new(None);

// CIGrTexture2dFile: +0x28 record, +0x30 archive file id, +0x34 state (7 = loaded),
// +0x7c size key. Record: +0x50 m_file, +0x58 m_fileHighRes, +0x60 flags, +0x94 users.
const REC_POOLED_OR_CUBE: u32 = 0x2006;

/// One frame of the model-texture reload. The file list is walked again each frame,
/// so nothing destroyed in between is ever touched.
fn reload_textures() {
    let mut g = TEX_DONE.lock().unwrap_or_else(|e| e.into_inner());
    let Some((done, left)) = g.as_mut() else { return };
    let (Some(&mgr_at), Some(&f)) = (FILE_MGR.get(), RELOAD_FN.get()) else {
        *g = None;
        return;
    };
    let mgr: usize = rd(mgr_at);
    if mgr == 0 {
        return;
    }
    let off = rd::<i32>(mgr) as usize;
    let mut picks = Vec::new();
    let mut waiting = 0u32;
    let mut p: usize = rd(mgr + 0x10);
    while p != 0 && p & 1 == 0 {
        let next: usize = rd(p + off + 8);
        let rec: usize = rd(p + 0x28);
        // Each record once, through its base file; only files the pack has an opinion on.
        if rec != 0 && rd::<usize>(rec + 0x50) == p && !done.contains(&rec) && rd::<u32>(rec + 0x60) & REC_POOLED_OR_CUBE == 0 {
            let hi: usize = rd(rec + 0x58);
            let wanted = crate::diag::wants_reload(rd(p + 0x30)) || (hi != 0 && crate::diag::wants_reload(rd(hi + 0x30)));
            if wanted {
                let ready = rd::<u32>(p + 0x34) == 7 && (hi == 0 || rd::<u32>(hi + 0x34) == 7) && rd::<u32>(rec + 0x94) == 0;
                if !ready {
                    waiting += 1;
                } else if picks.len() < TEXTURES_PER_FRAME {
                    picks.push((rec, p));
                } else {
                    waiting += 1;
                }
            }
        }
        p = next;
    }
    let f: TexReload = unsafe { std::mem::transmute(f) };
    for &(rec, file) in &picks {
        unsafe {
            // Size key changed, as with Texture Detail: the file is reset and read again.
            ((file + 0x7c) as *mut u32).write_unaligned(0);
            f(rec, 0);
        }
        done.insert(rec);
    }
    LAST_TEXTURES.store(done.len() as u32, Ordering::Relaxed);
    *left = left.saturating_sub(1);
    if (picks.is_empty() && waiting == 0) || *left == 0 {
        log!("reload: {} model textures reloaded, {waiting} left waiting", done.len());
        *g = None;
        crate::diag::clear_marked_if_idle();
        return;
    }
    TEX_WAITING.store(waiting, Ordering::Relaxed);
}

static ADVANCE: OnceLock<GenericDetour<Advance>> = OnceLock::new();
static SCHED: OnceLock<usize> = OnceLock::new();
/// Address of the composite list object (`{i32 link_offset; link sentinel}`).
static LIST: OnceLock<usize> = OnceLock::new();
/// Value the engine uses for "all components invalid" (read from the ctor), default 0x7fff.
static ALL_BITS: AtomicU32 = AtomicU32::new(0x7fff);

static REQUEST: AtomicBool = AtomicBool::new(false);
pub static LAST_SCHEDULED: AtomicU32 = AtomicU32::new(0);
/// Composites busy when the request came; retried on later frames.
static PENDING: std::sync::Mutex<Vec<(usize, u32)>> = std::sync::Mutex::new(Vec::new());

const OFF_VARIANT: usize = 0xa48; // shared variant/NPC composite: never touch
const OFF_INVALID: usize = 0xae4;
const OFF_STATE: usize = 0xb44;

/// Its final textures are complete (the rebuild only ever replaces them when done) and
/// outlive it through the shared refcount, so a rebuild in progress can simply stop:
/// report it as done. Any other composite goes through untouched.
unsafe extern "system" fn release(c: usize) -> u64 {
    if rd::<usize>(c + OFF_VARIANT) != 0 && rd::<u32>(c + OFF_FLAGS) & FLAG_SHARED_OWNER != 0 && rd::<u32>(c + OFF_STATE) != 7 && RELOADED.load(Ordering::Relaxed) {
        N_RELEASED_BUSY.fetch_add(1, Ordering::Relaxed);
        unsafe { ((c + OFF_STATE) as *mut u32).write_unaligned(7) };
    }
    unsafe { RELEASE_HOOK.get().unwrap().call(c) }
}
const MAX_COMPOSITES: usize = 8192;
const RETRY_FRAMES: u32 = 600;

/// Ask for a recomposition of every character on the next frame.
pub fn request() {
    request_after(std::time::Duration::ZERO);
}

/// Same, not before `wait` from now (a later call pushes it further).
pub fn request_after(wait: std::time::Duration) {
    *NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now() + wait);
    REQUEST.store(true, Ordering::Relaxed);
}

pub fn available() -> bool {
    ADVANCE.get().is_some() && SCHED.get().is_some() && LIST.get().is_some()
}

fn rd<T: Copy>(a: usize) -> T {
    unsafe { (a as *const T).read_unaligned() }
}

/// All composites in the engine's global list.
fn composites() -> Vec<usize> {
    let Some(&list) = LIST.get() else { return vec![] };
    let off = rd::<i32>(list) as usize;
    if off == 0 || off > 0x10000 {
        return vec![];
    }
    let mut out = Vec::new();
    // sentinel link at list+8: {prev, next}; next = object pointer, bit0 = end
    let mut p: usize = rd(list + 8 + 8);
    while p != 0 && p & 1 == 0 && out.len() < MAX_COMPOSITES {
        out.push(p);
        p = rd(p + off + 8);
    }
    out
}

/// Schedule `c` if it is idle (state 1 or 7); returns true when done.
fn try_schedule(c: usize) -> bool {
    if rd::<usize>(c + OFF_VARIANT) != 0 && !(LIVE_NPC.load(Ordering::Relaxed) && RELEASE_HOOK.get().is_some() && rd::<u32>(c + OFF_FLAGS) & FLAG_SHARED_OWNER != 0) {
        return true;
    }
    let state: u32 = rd(c + OFF_STATE);
    if state != 1 && state != 7 {
        return false;
    }
    let Some(&f) = SCHED.get() else { return true };
    unsafe {
        let inv = (c + OFF_INVALID) as *mut u32;
        inv.write_unaligned(inv.read_unaligned() | ALL_BITS.load(Ordering::Relaxed));
        let f: Schedule = std::mem::transmute(f);
        f(c);
    }
    true
}

unsafe extern "system" fn advance(a: u64, b: u64, c: u64, d: u64) -> u64 {
    let r = unsafe { ADVANCE.get().unwrap().call(a, b, c, d) };
    let due = REQUEST.load(Ordering::Relaxed)
        && LAST_REQUEST.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|t| t.elapsed().as_millis() >= MIN_REQUEST_GAP_MS)
        && NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|t| std::time::Instant::now() >= t);
    if due {
        REQUEST.store(false, Ordering::Relaxed);
        if !crate::overlay::enabled() {
            // Pieces served during the switch delay refilled it with HD buffers.
            crate::diag::drop_hd_cache();
        }
        *LAST_REQUEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        RELOADED.store(true, Ordering::Relaxed);
        let all = composites();
        let mut pend = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        pend.clear();
        let live_npc = LIVE_NPC.load(Ordering::Relaxed) && RELEASE_HOOK.get().is_some();
        let mut owners = 0u32;
        for c in all.iter().copied() {
            if rd::<usize>(c + OFF_VARIANT) != 0 {
                if !live_npc || rd::<u32>(c + OFF_FLAGS) & FLAG_SHARED_OWNER == 0 {
                    continue;
                }
                owners += 1;
            }
            pend.push((c, RETRY_FRAMES));
        }
        LAST_SCHEDULED.store(0, Ordering::Relaxed);
        log!(
            "reload: {} composites, {} to rebuild ({owners} NPC appearances), {} released busy so far",
            all.len(),
            pend.len(),
            N_RELEASED_BUSY.load(Ordering::Relaxed)
        );
        if LIVE_MODELS.load(Ordering::Relaxed) && FILE_MGR.get().is_some() {
            let mut t = TEX_DONE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((done, _)) = t.as_ref() {
                log!("reload: previous request stopped at {} textures, {} waiting", done.len(), TEX_WAITING.load(Ordering::Relaxed));
            }
            LAST_TEXTURES.store(0, Ordering::Relaxed);
            *t = Some((Default::default(), TEXTURE_FRAMES));
        }
    } else {
        let mut pend = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if !pend.is_empty() {
            // a composite may have been destroyed meanwhile: only touch live ones
            let live: std::collections::HashSet<usize> = composites().into_iter().collect();
            let mut budget = COMPOSITES_PER_FRAME;
            pend.retain_mut(|(c, left)| {
                if !live.contains(c) {
                    return false;
                }
                if budget == 0 {
                    return true;
                }
                if try_schedule(*c) {
                    budget -= 1;
                    LAST_SCHEDULED.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                *left -= 1;
                *left > 0
            });
        }
    }
    reload_textures();
    log_stats();
    probe_mod_vram();
    let waiting = !PENDING.lock().unwrap_or_else(|e| e.into_inner()).is_empty() || TEX_DONE.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    BUSY.store(waiting || REQUEST.load(Ordering::Relaxed), Ordering::Relaxed);
    r
}

/// Resolves a `call rel32` (E8) or `jmp rel32` (E9) target.
fn rel32_target(at: usize) -> usize {
    (at as isize + 5 + rd::<i32>(at + 1) as isize) as usize
}

pub fn install(img: &Image) {
    let (Some(site), Some(link), Some(sched)) = (img.find(&VIEW_ADVANCE), img.find(&LIST_LINK), img.find(&SCHEDULE)) else {
        log!("reload: a signature is missing; 'apply now' disabled");
        return;
    };
    // texture-advance call at site+12, usually a thunk `jmp real`
    let mut target = rel32_target(site + 12);
    if rd::<u8>(target) == 0xE9 {
        target = rel32_target(target);
    }
    // lea rcx,[rip+disp] at link+0x26
    if rd::<[u8; 3]>(link + 0x26) != [0x48, 0x8D, 0x0D] {
        log!("reload: composite list lea not where expected; disabled");
        return;
    }
    let list = (link as isize + 0x26 + 7 + rd::<i32>(link + 0x29) as isize) as usize;
    // "all invalid" value: first `mov eax,[rip+disp]` (8B 05) after the lea
    if let Some(p) = Pattern::parse("8B 05 ?? ?? ?? ??") {
        let code = unsafe { std::slice::from_raw_parts((link + 0x2d) as *const u8, 0x40) };
        if let Some(&o) = p.find_all(code).first() {
            let a = (link + 0x2d + o) as isize + 6 + rd::<i32>(link + 0x2d + o + 2) as isize;
            let v: u32 = rd(a as usize);
            if v != 0 {
                ALL_BITS.store(v, Ordering::Relaxed);
            }
        }
    }
    let t: Advance = unsafe { std::mem::transmute(target) };
    match unsafe { GenericDetour::new(t, advance as Advance) } {
        Ok(d) => {
            let _ = ADVANCE.set(d);
            if let Err(e) = unsafe { ADVANCE.get().unwrap().enable() } {
                log!("reload: enable failed: {e}");
                return;
            }
        }
        Err(e) => {
            log!("reload: detour failed: {e}");
            return;
        }
    }
    let _ = SCHED.set(sched);
    let _ = LIST.set(list);
    // NPC reload needs the release guard; without it NPCs are simply left out.
    match img.find(&RELEASE) {
        Some(a) => {
            let t: Release = unsafe { std::mem::transmute(a) };
            match unsafe { GenericDetour::new(t, release as Release) } {
                Ok(d) => {
                    let _ = RELEASE_HOOK.set(d);
                    match unsafe { RELEASE_HOOK.get().unwrap().enable() } {
                        Ok(()) => log!("reload: composite release guarded at rva {:#x}", img.rva(a)),
                        Err(e) => log!("reload: release guard enable failed: {e}"),
                    }
                }
                Err(e) => log!("reload: release guard failed: {e}"),
            }
        }
        None => log!("reload: composite release not found; NPCs left out of live reload"),
    }
    // Model textures: optional, the character reload works without them.
    match (img.find(&SET_QUALITY), img.find(&TEX_RELOAD)) {
        (Some(q), Some(t)) if rd::<[u8; 3]>(q + 0x47) == [0x48, 0x8B, 0x2D] => {
            let _ = FILE_MGR.set((q as isize + 0x47 + 7 + rd::<i32>(q + 0x4A) as isize) as usize);
            let _ = RELOAD_FN.set(t);
            log!("reload: model textures via rva {:#x}, file manager rva {:#x}", img.rva(t), img.rva(*FILE_MGR.get().unwrap()));
        }
        _ => log!("reload: model texture reload not available in this build"),
    }
    log!(
        "reload: frame hook rva {:#x}, list rva {:#x} (link offset {:#x}), schedule rva {:#x}, all-bits {:#x}",
        img.rva(target),
        img.rva(list),
        rd::<i32>(list),
        img.rva(sched),
        ALL_BITS.load(Ordering::Relaxed)
    );
}
