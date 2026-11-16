//! Armor/skin/face composite at 2× or 4×, chosen per character
//! (see docs/texture-reverse-engineering.md, "Armor composition" and "Per-character scale").
//!
//! The game composes each character on the CPU into a fixed-size atlas (1024², or
//! 512² at low quality) using fixed pixel rectangles per region. With HD sources
//! the atlas, the scratch buffers, the piece slots and every rectangle are scaled.
//! The scale is per composite: 4× for the local player (or every player when the
//! `composite_4x_all` option is on), 2× for everyone else. It is decided in the
//! composite constructors and made current (thread-local) in the two entry points
//! every composite step runs under (atlas creation 0x12d8fc0, Update 0x12d25e0).
//! The texture reader learns the scale from the slot record (4096 wide = 4×).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

/// Default composite scale (the one used when no per-composite scale is known).
pub static SCALE: AtomicU32 = AtomicU32::new(2);
/// 4× for every player on screen, not just the local one (config `composite_4x_all`).
/// Read when a composite is created, so a change applies to characters built after it.
pub static ALL_4X: AtomicBool = AtomicBool::new(false);
/// On the character select (from the start of the game until a composite of a character in the world, and again when a preview without
/// a character is built): no 4x at all there, neither composites nor the local player's model textures. A 4x model texture loaded on the
/// select had its first resize refused (2048 -> 1024) and the scene stayed black, as with the 4x composites before.
pub static AT_SELECT: AtomicBool = AtomicBool::new(true);

/// Records where the game is from a character composite's constructor (`select` = no character and none controlled).
pub fn note_select(select: bool, ch: usize, ctrl: usize) {
    if select {
        AT_SELECT.store(true, Ordering::Relaxed);
    } else if ch != 0 || ctrl != 0 {
        AT_SELECT.store(false, Ordering::Relaxed);
    }
}
/// Composites built at 4× so far (overlay counter).
pub static N_4X: AtomicU32 = AtomicU32::new(0);

/// composite address -> scale (rewritten by every constructor: addresses are reused).
static MAP: Mutex<Option<HashMap<usize, u32>>> = Mutex::new(None);
thread_local! {
    static CUR: Cell<u32> = const { Cell::new(0) };
    static IN_SCRATCH: Cell<bool> = const { Cell::new(false) };
    static IN_LAYER: Cell<bool> = const { Cell::new(false) };
}

fn scale() -> u32 {
    match CUR.with(|c| c.get()) {
        0 => SCALE.load(Ordering::Relaxed),
        s => s,
    }
}

fn map_get(c: usize) -> u32 {
    MAP.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&c).copied()).unwrap_or(2)
}

fn map_set(c: usize, s: u32) {
    MAP.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(c, s);
}

/// Scale the composite was built at (2x/4x).
pub fn current_scale(c: usize) -> u32 {
    map_get(c)
}

/// Player composite constructed (called from the ctor hook in player_diag.rs).
/// `own`: the local player's model (ctor flag bit 0, set for the controlled
/// character and its UI previews, or the AvCharModel character == controlled).
/// Bit 3 (0x8) = half-resolution composite: never 4×.
/// `select`: built before any character exists in the world (the character select's previews): always 2×. A 4×
/// atlas there left the whole character select black; the world builds its own composites later.
pub fn on_player_ctor(composite: usize, flags: u32, own: bool, select: bool) -> u32 {
    let hd = if wants_4x(flags, own, select) {
        4
    } else {
        2
    };
    map_set(composite, hd);
    if hd == 4 {
        N_4X.fetch_add(1, Ordering::Relaxed);
    }
    hd
}

fn wants_4x(flags: u32, own: bool, select: bool) -> bool {
    ENABLED_4X.load(Ordering::Relaxed)
        && ALLOW_4X.load(Ordering::Relaxed)
        && scale_rule(flags, own, select, ALL_4X.load(Ordering::Relaxed))
}

/// The 4× rule without the switches: not half-resolution, not the character select, and the local player's (or
/// every player's with `composite_4x_all`).
fn scale_rule(flags: u32, own: bool, select: bool, all: bool) -> bool {
    flags & 8 == 0 && !select && (own || all)
}

/// Composite 4x switch (config `composite_4x=false` turns it off; default on).
pub static ALLOW_4X: AtomicBool = AtomicBool::new(true);

/// True when all per-composite hooks are in place (otherwise everything stays 2×).
static ENABLED_4X: AtomicBool = AtomicBool::new(false);

use retour::GenericDetour;
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};

use crate::game::Image;
use crate::log;
use crate::sig::Site;

pub const ATLAS_DIMS: Site = Site {
    name: "composite_atlas_dims",
    sig: "C7 01 00 04 00 00 48 8B C1 C7 41 04 00 04 00 00 85 D2 74 0D C7 01 00 02 00 00 C7 41 04 00 02 00 00 C3",
};
pub const SCRATCH: Site = Site {
    name: "composite_scratch",
    sig: "48 89 5C 24 08 57 48 83 EC 30 C7 44 24 58 00 04 00 00 41 8B D8 C7 44 24 5C 00 04 00 00 85 C9 74 10 C7 44 24 58 00 02 00 00 C7 44 24 5C 00 02 00 00",
};
pub const REGION_RECTS: Site = Site { name: "composite_region_rects", sig: "48 89 5C 24 20 41 56 48 83 EC 30 8B C1 49 8B D8 4C 8B 42 14" };

/// `CpsData` blit-rect set table: `entry(set)` returns a 0x28-byte entry whose
/// +0x08/+0x0C are the fixed slot dims every composite source is loaded into
/// (1024×1024). We hand back a private copy with the dims doubled.
pub const BLIT_SET: Site = Site { name: "composite_blit_set", sig: "48 89 5C 24 08 57 48 83 EC 20 8B D9 E8 ?? ?? ?? ?? 48 8B 90 D0 00 00 00 48 8B BA B8 00 00 00" };
type BlitSet = unsafe extern "system" fn(u32) -> *const u8;
static BLIT: OnceLock<GenericDetour<BlitSet>> = OnceLock::new();
static BLIT_COPIES: std::sync::Mutex<Vec<(u32, u32, Box<[u8; 0x28]>)>> = std::sync::Mutex::new(Vec::new());

unsafe extern "system" fn blit_set(set: u32) -> *const u8 {
    let p = unsafe { BLIT.get().unwrap().call(set) };
    if p.is_null() {
        return p;
    }
    let sc = scale();
    let mut copies = BLIT_COPIES.lock().unwrap_or_else(|e| e.into_inner());
    // one private copy per (set, scale): a 2× and a 4× composite never share it
    let i = match copies.iter().position(|c| c.0 == set && c.1 == sc) {
        Some(i) => i,
        None => {
            copies.push((set, sc, Box::new([0u8; 0x28])));
            copies.len() - 1
        }
    };
    let c = &mut copies[i].2;
    unsafe { std::ptr::copy_nonoverlapping(p, c.as_mut_ptr(), 0x28) };
    for off in [8usize, 12] {
        let v = u32::from_le_bytes(c[off..off + 4].try_into().unwrap());
        c[off..off + 4].copy_from_slice(&(v * sc).to_le_bytes());
    }
    c.as_ptr()
}

type RegionRects = unsafe extern "system" fn(u32, *mut u8, *mut u8, i32);
static RECTS: OnceLock<GenericDetour<RegionRects>> = OnceLock::new();

/// `rects(region, regionData, outList, half)`: outList = {+0x08 entries, +0x14 count},
/// entries are `{x0, y0, x1, y1}` u32 in 1024-space (halved when `half`).
unsafe extern "system" fn region_rects(region: u32, data: *mut u8, out: *mut u8, half: i32) {
    let before = if out.is_null() { 0 } else { unsafe { out.add(0x14).cast::<u32>().read() } };
    unsafe { RECTS.get().unwrap().call(region, data, out, half) };
    if out.is_null() {
        return;
    }
    unsafe {
        let after = out.add(0x14).cast::<u32>().read();
        let base = out.add(0x08).cast::<*mut u32>().read();
        if base.is_null() || after < before {
            return;
        }
        for i in before..after {
            let e = base.add(i as usize * 4);
            for k in 0..4 {
                let v = e.add(k);
                v.write(v.read() * scale());
            }
        }
    }
}

type F12 = unsafe extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type AtlasDims = unsafe extern "system" fn(*mut u32, u32) -> *mut u32;

/// `CpsComposite` atlas creation `(composite, low)`: every step below it uses this composite.
const CREATE_ATLAS: Site = Site { name: "composite_create_atlas", sig: "40 53 48 83 EC 20 48 8B D9 48 8D 4C 24 30 E8 ?? ?? ?? ?? 45 33 C0 48 8D" };
/// `CpsComposite::Update(composite, ...)`: scratch, layers, blits.
const UPDATE: Site = Site { name: "composite_update", sig: "40 53 56 48 83 EC 68 8B 81 44 0B 00" };
/// `CpsTex` buffer pool `(dims packed in a u64, fmt, flags, levels, ...)`.
const POOL: Site = Site { name: "composite_pool", sig: "48 8B C4 48 89 48 08 55 56 41 56 48" };
/// Layer load `(layer, i, &composite->a40, ...)` (11 args): loads each piece (CreateFromFile).
const LAYER: Site = Site {
    name: "composite_layer",
    sig: "48 89 5C 24 20 55 56 57 41 54 41 55 41 56 41 57 48 81 EC 30 01 00 00 48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 84 24 20 01 00 00 48 8B 84",
};
/// GrTex2d loader lookup `(table, file, internal flags) -> loader` (the key has no dims).
const LOOKUP: Site = Site { name: "grtex2d_loader_lookup", sig: "48 89 6C 24 20 56 41 56 41 57 48 83 EC 30 4C 8B" };
/// Data composite ctor (NPC/variant appearance, `+0xa48 != 0`): always 2×.
const DATA_CTOR: Site = Site {
    name: "composite_data_ctor",
    sig: "48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 41 54 41 55 41 56 41 57 48 83 EC 30 48 8D 05 ?? ?? ?? ?? 45 33 ED 48 89",
};

static H_ATLAS: OnceLock<GenericDetour<AtlasDims>> = OnceLock::new();
static H_SCRATCH: OnceLock<GenericDetour<F12>> = OnceLock::new();
static H_POOL: OnceLock<GenericDetour<F12>> = OnceLock::new();
static H_CREATE: OnceLock<GenericDetour<F12>> = OnceLock::new();
static H_UPDATE: OnceLock<GenericDetour<F12>> = OnceLock::new();
static H_LAYER: OnceLock<GenericDetour<F12>> = OnceLock::new();
static H_LOOKUP: OnceLock<GenericDetour<F12>> = OnceLock::new();
static H_DATA: OnceLock<GenericDetour<F12>> = OnceLock::new();
/// Data (NPC/variant) composites built so far.
pub static N_DATA: AtomicU32 = AtomicU32::new(0);
/// Shared piece loaders refused because they were made for the other scale.
pub static N_MISS: AtomicU32 = AtomicU32::new(0);

/// Runs `f` with CUR set to `s`, restoring the previous value (calls nest).
fn with_cur<R>(s: u32, f: impl FnOnce() -> R) -> R {
    let prev = CUR.with(|c| c.replace(s));
    let r = f();
    CUR.with(|c| c.set(prev));
    r
}

unsafe extern "system" fn atlas_dims(out: *mut u32, low: u32) -> *mut u32 {
    let r = unsafe { H_ATLAS.get().unwrap().call(out, low) };
    if !out.is_null() {
        let sc = scale();
        unsafe {
            out.write(out.read() * sc);
            out.add(1).write(out.add(1).read() * sc);
        }
    }
    r
}

unsafe extern "system" fn scratch(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    let prev = IN_SCRATCH.with(|x| x.replace(true));
    let r = unsafe { H_SCRATCH.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) };
    IN_SCRATCH.with(|x| x.set(prev));
    r
}

unsafe extern "system" fn pool(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    let mut a = a;
    if IN_SCRATCH.with(|x| x.replace(false)) {
        // only the pool call made directly by the scratch function: dims × scale
        let sc = scale() as u64;
        a = ((a & 0xFFFF_FFFF) * sc) | (((a >> 32) * sc) << 32);
    }
    unsafe { H_POOL.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) }
}

unsafe extern "system" fn create_atlas(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    with_cur(map_get(a as usize), || unsafe { H_CREATE.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) })
}

unsafe extern "system" fn update(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    with_cur(map_get(a as usize), || unsafe { H_UPDATE.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) })
}

unsafe extern "system" fn layer(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    let prev = IN_LAYER.with(|x| x.replace(true));
    let r = unsafe { H_LAYER.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) };
    IN_LAYER.with(|x| x.set(prev));
    r
}

/// Inside a layer load, a cached piece loader made for the other scale (its record
/// is 4096 wide for 4×, less for 2×) is not reused: the engine creates a new one.
unsafe extern "system" fn lookup(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    let r = unsafe { H_LOOKUP.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) };
    if r == 0 || !IN_LAYER.with(|x| x.get()) || (c as u32 & 0xFF) != 0x19 {
        return r;
    }
    let rec = unsafe { ((r as usize + 0x28) as *const usize).read() };
    if rec == 0 {
        return r;
    }
    let w = unsafe { ((rec + 0x48) as *const u32).read() };
    if w != 0 && (w >= 4096) != (scale() == 4) {
        N_MISS.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    r
}

unsafe extern "system" fn data_ctor(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64, k: u64, l: u64) -> u64 {
    map_set(a as usize, 2);
    let n = N_DATA.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 5 || n.is_power_of_two() {
        log!("composite: data (NPC/variant) composites so far: {n}");
    }
    with_cur(2, || unsafe { H_DATA.get().unwrap().call(a, b, c, d, e, f, g, h, i, j, k, l) })
}

/// Player composite ctor (hooked in player_diag.rs): the first atlas is created
/// inside the ctor, so the scale is stored and made current before the original runs.
pub fn player_ctor<R>(composite: usize, flags: u32, own: bool, select: bool, f: impl FnOnce() -> R) -> (R, u32) {
    let s = on_player_ctor(composite, flags, own, select);
    (with_cur(s, f), s)
}

fn detour<T: retour::Function + Copy>(img: &Image, site: &Site, cell: &OnceLock<GenericDetour<T>>, hook: T) -> bool {
    let Some(addr) = img.find(site) else {
        log!("composite: {} not found", site.name);
        return false;
    };
    let target: T = unsafe { T::from_ptr(addr as *const ()) };
    match unsafe { GenericDetour::new(target, hook) } {
        Ok(d) => {
            let _ = cell.set(d);
            true
        }
        Err(e) => {
            log!("composite: {} detour failed: {e}", site.name);
            false
        }
    }
}

/// Per-composite scale hooks. All or nothing: returns false (and enables none) if
/// any is missing, so the caller falls back to the fixed 2× patch.
fn install_scaled(img: &Image) -> bool {
    let ok = detour(img, &ATLAS_DIMS, &H_ATLAS, atlas_dims as AtlasDims)
        && detour(img, &SCRATCH, &H_SCRATCH, scratch as F12)
        && detour(img, &POOL, &H_POOL, pool as F12)
        && detour(img, &CREATE_ATLAS, &H_CREATE, create_atlas as F12)
        && detour(img, &UPDATE, &H_UPDATE, update as F12)
        && detour(img, &LAYER, &H_LAYER, layer as F12)
        && detour(img, &LOOKUP, &H_LOOKUP, lookup as F12)
        && detour(img, &DATA_CTOR, &H_DATA, data_ctor as F12);
    if !ok {
        return false;
    }
    let res = unsafe {
        [
            H_POOL.get().unwrap().enable(),
            H_SCRATCH.get().unwrap().enable(),
            H_LOOKUP.get().unwrap().enable(),
            H_LAYER.get().unwrap().enable(),
            H_DATA.get().unwrap().enable(),
            H_UPDATE.get().unwrap().enable(),
            H_CREATE.get().unwrap().enable(),
            H_ATLAS.get().unwrap().enable(),
        ]
    };
    if let Some(Err(e)) = res.iter().find(|r| r.is_err()) {
        log!("composite: enable failed: {e}");
        unsafe {
            let _ = H_ATLAS.get().unwrap().disable();
            let _ = H_CREATE.get().unwrap().disable();
            let _ = H_UPDATE.get().unwrap().disable();
            let _ = H_DATA.get().unwrap().disable();
            let _ = H_LAYER.get().unwrap().disable();
            let _ = H_LOOKUP.get().unwrap().disable();
            let _ = H_SCRATCH.get().unwrap().disable();
            let _ = H_POOL.get().unwrap().disable();
        }
        return false;
    }
    true
}

/// Writes `new` over the u32 immediate at `addr` if it currently holds `old`.
unsafe fn patch_u32(addr: usize, old: u32, new: u32) -> bool {
    unsafe {
        let p = addr as *mut u32;
        if p.read_unaligned() != old {
            return false;
        }
        let mut prev = 0u32;
        if VirtualProtect(p.cast(), 4, PAGE_EXECUTE_READWRITE, &mut prev) == 0 {
            return false;
        }
        p.write_unaligned(new);
        VirtualProtect(p.cast(), 4, prev, &mut prev);
        true
    }
}

pub fn install(img: &Image) -> bool {
    let (Some(atlas), Some(scratch), Some(rects), Some(blit)) =
        (img.find(&ATLAS_DIMS), img.find(&SCRATCH), img.find(&REGION_RECTS), img.find(&BLIT_SET))
    else {
        log!("composite 2x: a signature is missing; armor composite stays at 1024");
        return false;
    };
    // Hook first: if it fails nothing has been patched yet.
    let target: RegionRects = unsafe { std::mem::transmute(rects) };
    let det = match unsafe { GenericDetour::new(target, region_rects as RegionRects) } {
        Ok(d) => d,
        Err(e) => {
            log!("composite 2x: detour failed: {e}");
            return false;
        }
    };
    let _ = RECTS.set(det);
    let bt: BlitSet = unsafe { std::mem::transmute(blit) };
    let bdet = match unsafe { GenericDetour::new(bt, blit_set as BlitSet) } {
        Ok(d) => d,
        Err(e) => {
            log!("composite 2x: blit-set detour failed: {e}");
            return false;
        }
    };
    let _ = BLIT.set(bdet);
    if let Err(e) = unsafe { RECTS.get().unwrap().enable() } {
        log!("composite 2x: enable failed: {e}");
        return false;
    }
    if let Err(e) = unsafe { BLIT.get().unwrap().enable() } {
        log!("composite 2x: blit-set enable failed: {e}");
        let _ = unsafe { RECTS.get().unwrap().disable() };
        return false;
    }
    // Per-composite scale (4× for the local player). If any hook is missing,
    // patch the constants to a fixed 2× like before (no 4× anywhere).
    let scaled = install_scaled(img);
    ENABLED_4X.store(scaled, Ordering::Relaxed);
    let ok = scaled
        || unsafe {
            let (hi, lo) = (0x400 * scale(), 0x200 * scale());
            patch_u32(atlas + 2, 0x400, hi)
                & patch_u32(atlas + 12, 0x400, hi)
                & patch_u32(atlas + 22, 0x200, lo)
                & patch_u32(atlas + 29, 0x200, lo)
                & patch_u32(scratch + 14, 0x400, hi)
                & patch_u32(scratch + 25, 0x400, hi)
                & patch_u32(scratch + 37, 0x200, lo)
                & patch_u32(scratch + 45, 0x200, lo)
        };
    log!("composite: per-character scale {}", if scaled { "ON (4x local player, 2x others)" } else { "OFF (fixed 2x)" });
    log!(
        "composite {}x: atlas rva {:#x}, scratch rva {:#x}, rects rva {:#x}, blit-set rva {:#x}, patched={ok}",
        scale(),
        img.rva(atlas),
        img.rva(scratch),
        img.rva(rects),
        img.rva(blit)
    );
    ok
}

#[cfg(test)]
mod scale_tests {
    use super::scale_rule;

    #[test]
    fn character_select_previews_stay_2x() {
        // World: the local player (0x213) gets 4x, others too with composite_4x_all.
        assert!(scale_rule(0x213, true, false, false));
        assert!(scale_rule(0x12, false, false, true));
        assert!(!scale_rule(0x12, false, false, false));
        // Character select (0x3, no character yet): never 4x, whatever the switches.
        assert!(!scale_rule(0x3, true, true, true));
        assert!(!scale_rule(0x3, true, true, false));
        // Half-resolution composites: never 4x.
        assert!(!scale_rule(0x8 | 0x1, true, false, true));
    }

    #[test]
    fn the_character_select_has_no_4x_until_a_character_is_in_the_world() {
        use super::{AT_SELECT, note_select};
        use std::sync::atomic::Ordering::Relaxed;
        AT_SELECT.store(true, Relaxed);
        note_select(true, 0, 0);
        assert!(AT_SELECT.load(Relaxed), "a preview without a character");
        note_select(false, 0x1000, 0);
        assert!(!AT_SELECT.load(Relaxed), "the world: a character");
        note_select(false, 0, 0x2000);
        assert!(!AT_SELECT.load(Relaxed));
        note_select(true, 0, 0);
        assert!(AT_SELECT.load(Relaxed), "back on the character select");
    }
}
