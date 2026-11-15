//! Forced anisotropic filtering and mip bias for the game's WORLD texture samplers. See `docs/aa-aniso.md`.
//!
//! HOW. The `ID3D11DeviceContext::PSSetSamplers` call is hooked (`d3d::h_ps_set_samplers`). Every sampler the engine binds
//! is looked up, by its pointer, in a small cache of "what to bind instead": a modified copy of the sampler state
//! (ANISOTROPIC filter, `MaxAnisotropy` = `aniso_level`, `MipLODBias` += `mip_bias`) or nothing. A cache miss reads the
//! sampler's desc (`GetDesc`), decides with `plan` and creates the copy once (`CreateSamplerState` on the sampler's own
//! device); a hit is a few instructions. The engine's objects are never modified, only what is bound differs.
//!
//! WHY NOT a `CreateSamplerState` hook: the mod loads before the device exists, but the hook of a device method is
//! installed lazily (when the first hooked call reaches `discover_device`), and the engine may create its samplers
//! before that. Binding-time substitution does not depend on the order, also handles a changed `aniso_level` live
//! (the copies are rebuilt lazily when the setting changes), and leaves a vanilla emulation (`enabled=0`) bit-identical
//! (nothing is substituted while the mod is off).
//!
//! WORLD SAMPLER OR NOT. Decided from the desc alone (`plan`):
//! - comparison samplers (shadow maps, `Filter & 0x80`) are never touched;
//! - samplers that cannot use mips (`MaxLOD <= 0.5`: UI atlases, the overlay's font sampler, 1:1 fullscreen passes) are not touched;
//! - samplers that clamp (or border) on every axis are not touched: that is what the post chain, the bloom chain, SMAA and
//!   every fullscreen pass use, world textures wrap or mirror (tiling terrain, props, characters);
//! - only the filters MIN_MAG_MIP_LINEAR (trilinear) and MIN_MAG_LINEAR_MIP_POINT (bilinear + point mips) become
//!   ANISOTROPIC; point filters stay point; an engine sampler that is ANISOTROPIC already is raised to the configured level
//!   when it is lower;
//! - the mod's own passes (`ignoring()` or inside a hook) are passed through untouched.
//!
//! Anisotropic filtering only samples more along the stretched direction of oblique surfaces; it never reveals anything the
//! engine did not draw. The mip bias is added to the engine's own bias and clamped to D3D's range.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::d3d::{IgnoreScope, Obj, release, vfn, vt};
use crate::log;

pub const LEVEL_RANGE: (u32, u32) = (0, 16);
pub const DEFAULT_LEVEL: u32 = 16;
pub const BIAS_RANGE: (f32, f32) = (-1.0, 0.5);
pub const DEFAULT_BIAS: f32 = 0.0;
/// Trilinear samplers that tile (wrap / mirror) on U or V.
pub const FILTER_TRILINEAR: u32 = 1;
pub const FILTER_MIP_POINT: u32 = 2;
pub const FILTER_ANISO: u32 = 4;
/// Trilinear samplers that clamp on both U and V (only W tiles: for a 2D texture that is a clamped atlas).
pub const FILTER_TRILINEAR_CLAMPED: u32 = 8;
/// Only samplers the engine already made anisotropic are changed by default (their level is raised). Forcing anisotropy on
/// the trilinear samplers and then on the bilinear point-mip ones drew dashed dark lines along
/// the mesh edges of floors, walls and characters: A/B in the game, live, the lines come and go with those bits.
pub const DEFAULT_FILTERS: u32 = FILTER_ANISO;
/// The previous default (point-mip + anisotropic), written into existing config files: read as the current default.
pub const OLD_DEFAULT_FILTERS: u32 = FILTER_MIP_POINT | FILTER_ANISO;

const FILTER_MIN_MAG_MIP_LINEAR: u32 = 0x15;
const FILTER_MIN_MAG_LINEAR_MIP_POINT: u32 = 0x14;
const FILTER_ANISOTROPIC: u32 = 0x55;
const FILTER_COMPARISON_BIT: u32 = 0x80;
const ADDR_WRAP: u32 = 1;
const ADDR_MIRROR: u32 = 2;
const ADDR_MIRROR_ONCE: u32 = 5;

const DEV_CREATE_SAMPLER_STATE: usize = 23;
const SAMPLER_GET_DESC: usize = 7; // IUnknown 3 + DeviceChild 4

static LEVEL: AtomicU32 = AtomicU32::new(DEFAULT_LEVEL);
static BIAS: AtomicU32 = AtomicU32::new(0); // 0.0f32
/// Bumped whenever a setting changes: cache entries of an older generation are rebuilt on next use.
static GEN: AtomicU32 = AtomicU32::new(1);

static CALLS: AtomicU64 = AtomicU64::new(0);
static SEEN: AtomicU32 = AtomicU32::new(0);
static CHANGED: AtomicU32 = AtomicU32::new(0);
static SKIP_CMP: AtomicU32 = AtomicU32::new(0);
static SKIP_NOMIP: AtomicU32 = AtomicU32::new(0);
static SKIP_CLAMP: AtomicU32 = AtomicU32::new(0);
static SKIP_KIND: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);
/// Which engine filters may be changed (`aniso_filters`): 1 = trilinear, 2 = bilinear with point mips, 4 = anisotropic already.
static FILTERS: AtomicU32 = AtomicU32::new(DEFAULT_FILTERS);
/// Changed samplers logged so far (the log gets at most `MAX_LOGGED` desc lines).
static LOGGED: AtomicU32 = AtomicU32::new(0);
const MAX_LOGGED: u32 = 64;

/// Clamps to the range (0 and 1 both mean off).
pub fn clamp_level(v: u32) -> u32 {
    v.clamp(LEVEL_RANGE.0, LEVEL_RANGE.1)
}

/// Clamps to -1..=0.5; NaN gives the default.
pub fn clamp_bias(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_BIAS } else { v.clamp(BIAS_RANGE.0, BIAS_RANGE.1) }
}

fn reset_counters() {
    for c in [&SEEN, &CHANGED, &SKIP_CMP, &SKIP_NOMIP, &SKIP_CLAMP, &SKIP_KIND, &FAILED] {
        c.store(0, Relaxed);
    }
}

pub fn set_level(v: u32) -> u32 {
    let v = clamp_level(v);
    if LEVEL.swap(v, Relaxed) != v {
        GEN.fetch_add(1, Relaxed);
        reset_counters();
    }
    v
}

/// Sets the filter classes that may be changed (bits of `FILTER_*`, 0..15).
pub fn set_filters(v: u32) -> u32 {
    let v = v & 15;
    if FILTERS.swap(v, Relaxed) != v {
        GEN.fetch_add(1, Relaxed);
        reset_counters();
    }
    v
}

pub fn filters() -> u32 {
    FILTERS.load(Relaxed)
}

/// The `FILTER_*` class bit of an engine sampler (0 = not a world filter).
pub fn filter_class(d: &Desc) -> u32 {
    match d.filter {
        FILTER_MIN_MAG_MIP_LINEAR if !tiles(d.u) && !tiles(d.v) => FILTER_TRILINEAR_CLAMPED,
        FILTER_MIN_MAG_MIP_LINEAR => FILTER_TRILINEAR,
        FILTER_MIN_MAG_LINEAR_MIP_POINT => FILTER_MIP_POINT,
        FILTER_ANISOTROPIC => FILTER_ANISO,
        _ => 0,
    }
}

pub fn level() -> u32 {
    LEVEL.load(Relaxed)
}

pub fn set_bias(v: f32) -> f32 {
    let v = clamp_bias(v);
    if BIAS.swap(v.to_bits(), Relaxed) != v.to_bits() {
        GEN.fetch_add(1, Relaxed);
        reset_counters();
    }
    v
}

pub fn bias() -> f32 {
    f32::from_bits(BIAS.load(Relaxed))
}

/// Applies the config at start.
pub fn init(level: u32, bias: f32, filters: u32) {
    set_level(level);
    set_filters(filters);
    set_bias(bias);
    log!("aniso: configured (level {}, mip bias {}, filters {})", self::level(), self::bias(), self::filters());
}

/// True when anything would be substituted right now (a setting is on and the mod is not switched off).
fn active() -> bool {
    active_for(level(), bias(), crate::gate::user_enabled())
}

/// Pure form of `active` (tested): nothing is substituted while the mod is switched off or both settings are neutral.
pub fn active_for(level: u32, bias: f32, user_enabled: bool) -> bool {
    user_enabled && (level >= 2 || bias != 0.0)
}

// ---- the decision (pure) ---------------------------------------------------------------------------------

/// `D3D11_SAMPLER_DESC` (52 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Desc {
    pub filter: u32,
    pub u: u32,
    pub v: u32,
    pub w: u32,
    pub bias: f32,
    pub max_aniso: u32,
    pub cmp: u32,
    pub border: [f32; 4],
    pub min_lod: f32,
    pub max_lod: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    /// Bind this modified desc instead.
    Change(Desc),
    /// A world sampler that needs no change (already at the level, bias 0).
    Same,
    Comparison,
    NoMips,
    ClampOnly,
    OtherFilter,
}

fn tiles(a: u32) -> bool {
    matches!(a, ADDR_WRAP | ADDR_MIRROR | ADDR_MIRROR_ONCE)
}

/// What to do with a sampler of desc `d` for the settings `level` (0/1 = off) and `bias`.
pub fn plan(d: &Desc, level: u32, bias: f32) -> Verdict {
    if d.filter & FILTER_COMPARISON_BIT != 0 {
        return Verdict::Comparison;
    }
    // A sampler whose LOD range cannot leave mip 0 has nothing to gain (UI, overlay, 1:1 passes).
    if !(d.max_lod > 0.5) || d.min_lod >= d.max_lod {
        return Verdict::NoMips;
    }
    if !(tiles(d.u) || tiles(d.v) || tiles(d.w)) {
        return Verdict::ClampOnly;
    }
    let world = matches!(d.filter, FILTER_MIN_MAG_MIP_LINEAR | FILTER_MIN_MAG_LINEAR_MIP_POINT | FILTER_ANISOTROPIC);
    if !world {
        return Verdict::OtherFilter;
    }
    let level = clamp_level(level);
    let mut n = *d;
    if level >= 2 {
        n.filter = FILTER_ANISOTROPIC;
        n.max_aniso = if d.filter == FILTER_ANISOTROPIC { d.max_aniso.max(level).min(16) } else { level };
    }
    let bias = clamp_bias(bias);
    if bias != 0.0 && d.bias.is_finite() {
        n.bias = (d.bias + bias).clamp(-16.0, 15.99);
    }
    if n == *d { Verdict::Same } else { Verdict::Change(n) }
}

// ---- the cache -------------------------------------------------------------------------------------------

const CAP: usize = 8192;
/// Stop adding new samplers beyond this many (the table never gets more than half full).
const MAX_ENTRIES: usize = CAP / 2;

#[derive(Clone, Copy)]
struct Slot {
    /// The engine's sampler (we hold a reference so the address cannot be reused). 0 = empty slot.
    key: usize,
    epoch: u32,
    /// The modified copy we own (0 = bind the original).
    modified: usize,
}

struct Cache {
    slots: Vec<Slot>,
    used: usize,
}

impl Cache {
    const fn new() -> Cache {
        Cache { slots: Vec::new(), used: 0 }
    }

    fn index(key: usize) -> usize {
        (((key >> 4) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize & (CAP - 1)
    }

    /// Slot position of `key` (or of the empty slot where it would go); None when the table is full.
    fn find(&mut self, key: usize) -> Option<usize> {
        if self.slots.is_empty() {
            self.slots = vec![Slot { key: 0, epoch: 0, modified: 0 }; CAP];
        }
        let mut i = Self::index(key);
        for _ in 0..CAP {
            let s = self.slots[i].key;
            if s == key || s == 0 {
                return Some(i);
            }
            i = (i + 1) & (CAP - 1);
        }
        None
    }
}

static CACHE: Mutex<Cache> = Mutex::new(Cache::new());

unsafe fn addref(o: Obj) {
    unsafe {
        let f: unsafe extern "system" fn(Obj) -> u32 = std::mem::transmute(vfn(o, 1));
        f(o);
    }
}

unsafe fn get_desc(smp: Obj) -> Desc {
    unsafe {
        let mut d: Desc = std::mem::zeroed();
        let f: unsafe extern "system" fn(Obj, *mut Desc) = std::mem::transmute(vfn(smp, SAMPLER_GET_DESC));
        f(smp, &mut d);
        d
    }
}

/// Creates a sampler state with `d` on the device that owns `like`. Null on failure.
unsafe fn create_like(like: Obj, d: &Desc) -> Obj {
    unsafe {
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(like, vt::CHILD_GET_DEVICE));
        get_device(like, &mut dev);
        if dev.is_null() {
            return std::ptr::null_mut();
        }
        let _ig = IgnoreScope::new();
        let create: unsafe extern "system" fn(Obj, *const Desc, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, DEV_CREATE_SAMPLER_STATE));
        let mut out: Obj = std::ptr::null_mut();
        let hr = create(dev, d, &mut out);
        release(dev);
        if hr < 0 { std::ptr::null_mut() } else { out }
    }
}

/// Evaluates one sampler for the current settings and returns the modified copy (null = bind the original).
unsafe fn evaluate(smp: Obj, level: u32, bias: f32) -> Obj {
    unsafe {
        SEEN.fetch_add(1, Relaxed);
        let d = get_desc(smp);
        let verdict = match plan(&d, level, bias) {
            Verdict::Change(_) if filter_class(&d) & filters() == 0 => Verdict::Same,
            v => v,
        };
        if let Verdict::Change(n) = verdict
            && LOGGED.fetch_add(1, Relaxed) < MAX_LOGGED
        {
            log!(
                "aniso: sampler {:p} filter {:#x} address {}/{}/{} lod {}..{} bias {} aniso {} -> filter {:#x} aniso {} bias {}",
                smp, d.filter, d.u, d.v, d.w, d.min_lod, d.max_lod, d.bias, d.max_aniso, n.filter, n.max_aniso, n.bias
            );
        }
        match verdict {
            Verdict::Change(n) => {
                let m = create_like(smp, &n);
                if m.is_null() {
                    if FAILED.fetch_add(1, Relaxed) == 0 {
                        log!("aniso: CreateSamplerState failed for a modified copy (filter {:#x}, aniso {}); that sampler stays as the engine made it", n.filter, n.max_aniso);
                    }
                } else {
                    CHANGED.fetch_add(1, Relaxed);
                }
                m
            }
            Verdict::Same => std::ptr::null_mut(),
            Verdict::Comparison => {
                SKIP_CMP.fetch_add(1, Relaxed);
                std::ptr::null_mut()
            }
            Verdict::NoMips => {
                SKIP_NOMIP.fetch_add(1, Relaxed);
                std::ptr::null_mut()
            }
            Verdict::ClampOnly => {
                SKIP_CLAMP.fetch_add(1, Relaxed);
                std::ptr::null_mut()
            }
            Verdict::OtherFilter => {
                SKIP_KIND.fetch_add(1, Relaxed);
                std::ptr::null_mut()
            }
        }
    }
}

/// Fills `out[..num]` with what to bind instead of `smp[..num]` (`num` <= 16). True when at least one entry differs
/// from the engine's. Never waits for a lock held by another thread.
///
/// # Safety
/// `smp` must point to `num` sampler pointers (null allowed) that are live `ID3D11SamplerState` objects.
pub unsafe fn translate(smp: *const Obj, num: u32, out: &mut [Obj; 16]) -> bool {
    CALLS.fetch_add(1, Relaxed);
    if num == 0 || num > 16 || smp.is_null() || !active() {
        return false;
    }
    let Ok(mut cache) = CACHE.try_lock() else { return false };
    let (level, bias, epoch) = (level(), bias(), GEN.load(Relaxed));
    let mut changed = false;
    for i in 0..num as usize {
        let p = unsafe { *smp.add(i) };
        out[i] = p;
        if p.is_null() {
            continue;
        }
        let Some(pos) = cache.find(p as usize) else { continue };
        let slot = cache.slots[pos];
        if slot.key == p as usize && slot.epoch == epoch {
            if slot.modified != 0 {
                out[i] = slot.modified as Obj;
                changed = true;
            }
            continue;
        }
        if slot.key == 0 {
            if cache.used >= MAX_ENTRIES {
                continue;
            }
            // Keep the engine's sampler alive while we remember its address.
            unsafe { addref(p) };
            cache.used += 1;
        } else if slot.modified != 0 {
            unsafe { release(slot.modified as Obj) };
        }
        let m = unsafe { evaluate(p, level, bias) };
        cache.slots[pos] = Slot { key: p as usize, epoch, modified: m as usize };
        if !m.is_null() {
            out[i] = m;
            changed = true;
        }
    }
    changed
}

/// Dev capture / overlay line.
pub fn status_text() -> String {
    let (l, b) = (level(), bias());
    if l < 2 && b == 0.0 {
        return "off (aniso_level 0/1 and mip_bias 0: the game's own samplers)".to_string();
    }
    if !crate::gate::user_enabled() {
        return "off (mod disabled)".to_string();
    }
    if !crate::d3d::ps_set_samplers_hooked() {
        return "not hooked (PSSetSamplers); samplers stay the game's own".to_string();
    }
    let seen = SEEN.load(Relaxed);
    format!(
        "{} mip bias {:+.2}: {} of {} samplers changed (skipped: {} comparison, {} single-mip, {} clamp-only, {} point/other filter{}); {} PSSetSamplers calls",
        if l >= 2 { format!("{l}x anisotropic,") } else { "aniso off,".to_string() },
        b,
        CHANGED.load(Relaxed),
        seen,
        SKIP_CMP.load(Relaxed),
        SKIP_NOMIP.load(Relaxed),
        SKIP_CLAMP.load(Relaxed),
        SKIP_KIND.load(Relaxed),
        if FAILED.load(Relaxed) > 0 { format!(", {} failed", FAILED.load(Relaxed)) } else { String::new() },
        CALLS.load(Relaxed)
    )
}

#[cfg(test)]
#[path = "aniso_tests.rs"]
mod tests;
