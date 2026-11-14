//! Screen-space reflections (M4). See `docs/ssr.md`.
//!
//! Where: at the light-adjust draw (`datapath::bind_current` calls `on_light_adjust` once per frame). Inputs: the
//! engine's linear depth (`ao::depth_resource`) and the scene colour (PS SRV slot 0 at that moment). Three draws
//! (trace, blur, blur) into the module's own half-resolution RGBA16F targets; the finished texture is bound as `t8`
//! and the strength as `b7` of the light-adjust substitute, which adds `refl.rgb * strength` to the linear colour.
//! Nothing is drawn into the engine's targets.
//!
//! Materials (see `docs/ssr.md`, "Materials"): the trace reads the raw alpha of the scene colour at the receiving
//! pixel (the engine's 3-class material mask) and drops the reflection for classes cleared in `classes`; the built-in
//! heuristics (`smart`) reduce it on green-dominant (vegetation) and noisy (rough) receivers. The matte gate
//! (`ssr_matte_cut`, a 1x1 frame-mean pass before the trace) keeps the score only where the receiver still looks like
//! water / glass / ice under the frame's own light; a 64x36 telemetry grid is read back now and then for the status line.
//!
//! Live settings are atomics copied into constant buffers: moving a slider never compiles anything. The
//! shaders are compiled once, on a background thread started by `init` (the render thread never calls D3DCompile).
//! Strength 0 turns the whole feature off: no passes, and the depth consumer bit is released.
//!
//! Plain decisions (`clamp_*`, `cb_values`, `half_size`) are host-testable; `Ssr` talks to D3D through raw vtables
//! like `ao.rs` and is exercised on WARP by `ssr_tests.rs`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::passes::FMT_R16G16B16A16_FLOAT;
use crate::{ao, log, shaders};

pub const HLSL: &str = include_str!("../shaders/ssr.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_TRACE: &str = "ps_trace";
pub const PS_BLUR_H: &str = "ps_blur_h";
pub const PS_BLUR_V: &str = "ps_blur_v";

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 2.0);
pub const RANGE_RANGE: (f32, f32) = (0.25, 2.0);
pub const DEFAULT_STRENGTH: f32 = 0.5;
pub const DEFAULT_RANGE: f32 = 1.0;
pub const SMART_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_SMART: f32 = 1.0;
pub const HUE_RANGE: (f32, f32) = (0.0, 1.0);
pub const FLAT_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_HUE: f32 = 1.0;
pub const DEFAULT_FLAT: f32 = 1.0;
/// Water / glass boost (`ssr_water_boost`): 0 = the old trace, bit for bit.
pub const WATER_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_WATER: f32 = 0.6;
/// Depth look of water (`water_depth_look`): the receiver is darkened by up to 15 % x this where the reflection is strong.
pub const DEPTH_LOOK_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_DEPTH_LOOK: f32 = 0.3;
/// Matte gate (`ssr_matte_cut`, docs/ssr.md "Matte gate"): 0 = the reflectivity score as before, bit for bit; 1 = only
/// receivers that look like water / glass / ice under the frame's own light keep their score.
pub const MATTE_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_MATTE: f32 = 0.8;
/// Telemetry grid of `ps_stats` (one sample per texel) and how often it runs (frames).
pub const STATS_W: u32 = 64;
pub const STATS_H: u32 = 36;
const STATS_EVERY: u32 = 60;
const FMT_RGBA32F: u32 = 2;
/// Format of the water map written by the trace (x = water amount, y = ripple): R16G16_FLOAT.
const FMT_R16G16_FLOAT: u32 = 34;
/// Material class bits of `ssr_classes` (the class of the raw scene alpha: <= 0.241 low, <= 0.492 mid, above high).
pub const CLASS_LOW: u32 = 1;
pub const CLASS_MID: u32 = 2;
pub const CLASS_HIGH: u32 = 4;
pub const CLASSES_ALL: u32 = 7;

/// Fresnel at normal incidence (the floor of the term; it grows as `(1 - n.v)^4` towards grazing angles).
pub const FRESNEL_FLOOR: f32 = 0.04;
/// Ray length in depths of the reflecting pixel, per unit of the range slider.
pub const RAY_LENGTH: f32 = 3.0;
/// A ray that ends in sky reflects the sky colour at this fraction of a normal hit.
pub const SKY_WEIGHT: f32 = 0.35;
/// Largest linear colour a reflection can carry (no fireflies from lights).
pub const MAX_LINEAR: f32 = 3.0;

const SLOT_T: u32 = 8;
const SLOT_B: u32 = 7;

// ---- switches --------------------------------------------------------------------------------

static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static BOUND: AtomicBool = AtomicBool::new(false);
static STRENGTH: AtomicU32 = AtomicU32::new(0x3f00_0000); // 0.5f32
static RANGE: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static SMART: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static HUE: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static FLAT: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static WATER: AtomicU32 = AtomicU32::new(0x3f19_999a); // 0.6f32
static DEPTH_LOOK: AtomicU32 = AtomicU32::new(0x3e99_999a); // 0.3f32
static CLASSES: AtomicU32 = AtomicU32::new(CLASSES_ALL);
static MATTE: AtomicU32 = AtomicU32::new(0x3f4c_cccd); // 0.8f32
static FRAMES: AtomicU32 = AtomicU32::new(0);
/// The last telemetry read back from the GPU (`ps_stats`), for the status line.
static STATS: Mutex<Option<MatteStats>> = Mutex::new(None);
static CUR: AtomicU8 = AtomicU8::new(0);
static LAST: AtomicU8 = AtomicU8::new(0);
const SEEN_APPLIED: u8 = 1;
const SEEN_NO_DEPTH: u8 = 2;
const SEEN_NO_SCENE: u8 = 4;
const SEEN_BAD_SIZE: u8 = 8;
const SEEN_NO_RUN: u8 = 16;

/// Clamps to 0..=2; NaN gives the default (0.5).
pub fn clamp_strength(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_STRENGTH } else { v.clamp(STRENGTH_RANGE.0, STRENGTH_RANGE.1) }
}

/// Clamps to 0.25..=2; NaN gives the default (1).
pub fn clamp_range(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_RANGE } else { v.clamp(RANGE_RANGE.0, RANGE_RANGE.1) }
}

/// Clamps to 0..=1; NaN gives the default (1).
pub fn clamp_smart(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_SMART } else { v.clamp(SMART_RANGE.0, SMART_RANGE.1) }
}

pub fn set_smart(v: f32) -> f32 {
    let v = clamp_smart(v);
    SMART.store(v.to_bits(), Relaxed);
    v
}

/// Clamps to 0..=1; NaN gives the default (1).
pub fn clamp_unit(v: f32) -> f32 {
    if v.is_nan() { 1.0 } else { v.clamp(0.0, 1.0) }
}

pub fn set_hue_filter(v: f32) -> f32 {
    let v = clamp_unit(v);
    HUE.store(v.to_bits(), Relaxed);
    v
}

pub fn hue_filter() -> f32 {
    f32::from_bits(HUE.load(Relaxed))
}

pub fn set_flatness(v: f32) -> f32 {
    let v = clamp_unit(v);
    FLAT.store(v.to_bits(), Relaxed);
    v
}

pub fn flatness() -> f32 {
    f32::from_bits(FLAT.load(Relaxed))
}

pub fn smart() -> f32 {
    f32::from_bits(SMART.load(Relaxed))
}

/// Clamps to 0..=1; NaN gives the default (0.6).
pub fn clamp_water(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_WATER } else { v.clamp(WATER_RANGE.0, WATER_RANGE.1) }
}

pub fn set_water_boost(v: f32) -> f32 {
    let v = clamp_water(v);
    WATER.store(v.to_bits(), Relaxed);
    v
}

pub fn water_boost() -> f32 {
    f32::from_bits(WATER.load(Relaxed))
}

/// Clamps to 0..=1; NaN gives the default (0.3).
pub fn clamp_depth_look(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_DEPTH_LOOK } else { v.clamp(DEPTH_LOOK_RANGE.0, DEPTH_LOOK_RANGE.1) }
}

pub fn set_depth_look(v: f32) -> f32 {
    let v = clamp_depth_look(v);
    DEPTH_LOOK.store(v.to_bits(), Relaxed);
    v
}

pub fn depth_look() -> f32 {
    f32::from_bits(DEPTH_LOOK.load(Relaxed))
}

/// Clamps to 0..=1; NaN gives the default (0.8).
pub fn clamp_matte(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_MATTE } else { v.clamp(MATTE_RANGE.0, MATTE_RANGE.1) }
}

pub fn set_matte_cut(v: f32) -> f32 {
    let v = clamp_matte(v);
    MATTE.store(v.to_bits(), Relaxed);
    v
}

pub fn matte_cut() -> f32 {
    f32::from_bits(MATTE.load(Relaxed))
}

/// Telemetry of the reflectivity score over the `STATS_W` x `STATS_H` grid (counts of samples).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MatteStats {
    pub samples: u32,
    /// Floors the trace scores (the floor test and the class gate passed).
    pub floors: u32,
    /// Floors with a score above 0 after the matte gate, split by the gate's class (water / glass / ice or anything else).
    pub reflective_water: u32,
    pub reflective_other: u32,
    /// Floors with a score above 0 without the matte gate (cut 0).
    pub would_reflect: u32,
    /// Of those, the ones the gate brought to 0.
    pub rejected: u32,
    /// The matte cut the sample was taken with.
    pub cut: f32,
}

/// Pure: counts the `ps_stats` samples (x floor, y score with the cut, z score without, w water part of the gloss).
pub fn summarize(samples: &[[f32; 4]], cut: f32) -> MatteStats {
    let mut s = MatteStats { samples: samples.len() as u32, cut, ..MatteStats::default() };
    let on = |v: f32| v.is_finite() && v > 0.01;
    for p in samples {
        if !(p[0] > 0.5) {
            continue;
        }
        s.floors += 1;
        let (with, without) = (on(p[1]), on(p[2]));
        if with {
            if p[3].is_finite() && p[3] > 0.5 { s.reflective_water += 1 } else { s.reflective_other += 1 }
        }
        if without {
            s.would_reflect += 1;
            if !with {
                s.rejected += 1;
            }
        }
    }
    s
}

/// Pure: the telemetry part of the status line.
pub fn stats_text(s: &MatteStats) -> String {
    let pct = |n: u32, d: u32| if d == 0 { 0.0 } else { 100.0 * n as f32 / d as f32 };
    format!(
        "matte cut {:.2}; sample {} pts: floors {:.1} %, reflective {} (water/glass {}, other {}) = {:.1} % of floors; the score alone would reflect {} ({:.1} % of floors), the matte gate rejected {} = {:.0} % of those",
        s.cut,
        s.samples,
        pct(s.floors, s.samples),
        s.reflective_water + s.reflective_other,
        s.reflective_water,
        s.reflective_other,
        pct(s.reflective_water + s.reflective_other, s.floors),
        s.would_reflect,
        pct(s.would_reflect, s.floors),
        s.rejected,
        pct(s.rejected, s.would_reflect)
    )
}

/// Keeps the three class bits.
pub fn set_classes(v: u32) -> u32 {
    let v = v & CLASSES_ALL;
    CLASSES.store(v, Relaxed);
    v
}

pub fn classes() -> u32 {
    CLASSES.load(Relaxed)
}

/// Sets the strength and the depth consumer bit with it (0 releases the depth capture for this feature).
pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    ao::set_depth_consumer(ao::DEPTH_FOR_SSR, v > 0.0);
    v
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Ssr, f32::from_bits(STRENGTH.load(Relaxed)))
}

pub fn set_range(v: f32) -> f32 {
    let v = clamp_range(v);
    RANGE.store(v.to_bits(), Relaxed);
    v
}

pub fn range() -> f32 {
    f32::from_bits(RANGE.load(Relaxed))
}

/// The pass should run: gate open (the AO's gate), strength above 0, no failure, not done this frame.
#[allow(dead_code)] // used by the tests
pub fn wants() -> bool {
    ao::enabled() && strength() > 0.0 && !FAILED.load(Relaxed) && !DONE.load(Relaxed)
}

/// A new frame starts (called from `recorder::frame_begin`).
pub fn frame_begin() {
    DONE.store(false, Relaxed);
    LAST.store(CUR.swap(0, Relaxed), Relaxed);
}

/// Overlay line.
pub fn status_text() -> String {
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    if strength() <= 0.0 {
        return "off (strength 0)".to_string();
    }
    if !ao::enabled() {
        return "waiting (classifier gate closed or mod disabled)".to_string();
    }
    let l = LAST.load(Relaxed);
    if l & SEEN_APPLIED != 0 {
        let st = match STATS.lock().ok().and_then(|g| *g) {
            Some(s) => stats_text(&s),
            None => format!("matte cut {:.2}; no sample read back yet", matte_cut()),
        };
        format!("applied (water boost {:.2}, water depth look {:.2}, smart {:.2}); {st}", water_boost(), depth_look(), smart())
    } else if l & SEEN_NO_DEPTH != 0 {
        "skipped: depth not found".to_string()
    } else if l & SEEN_NO_SCENE != 0 {
        "skipped: scene colour not bound".to_string()
    } else if l & SEEN_BAD_SIZE != 0 {
        "skipped: scene and depth sizes differ".to_string()
    } else if l & SEEN_NO_RUN != 0 {
        "skipped: could not run (see log)".to_string()
    } else if BLOBS.get().is_none() {
        "compiling shaders".to_string()
    } else {
        "waiting for a scene frame".to_string()
    }
}

// ---- constants --------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SsrParams {
    pub range: f32,
    pub width: u32,
    pub height: u32,
    /// Allowed material classes (bitmask, `CLASS_*`).
    pub classes: u32,
    /// Built-in heuristics: 0 off .. 1 full.
    pub smart: f32,
    /// Hue gate of the reflectivity score: 0 off .. 1 full.
    pub hue_filter: f32,
    /// Large-scale flatness requirement of the reflectivity score: 0 off .. 1 full.
    pub flatness: f32,
    /// Debug level 5: the alpha of the output is the reflectivity score (heat map) instead of the weight.
    pub debug_reflectivity: bool,
    /// Water / glass boost 0..1 (0 = the old trace).
    pub water_boost: f32,
    /// Water depth look 0..1 (needs the boost above 0).
    pub water_depth_look: f32,
    /// Matte gate 0..1 (0 = the score as before).
    pub matte_cut: f32,
    /// Also draw the telemetry grid (`ps_stats`) and queue its read-back this frame.
    pub stats: bool,
}

/// Size of the reflection targets: half resolution, rounded up.
pub fn half_size(w: u32, h: u32) -> (u32, u32) {
    (w.max(1).div_ceil(2), h.max(1).div_ceil(2))
}

/// The constant-buffer vectors of `ssr.hlsl` (see its header).
pub fn cb_values(p: &SsrParams) -> [[f32; 4]; 6] {
    let (w, h) = (p.width.max(1) as f32, p.height.max(1) as f32);
    let (hw, hh) = half_size(p.width, p.height);
    [
        [clamp_range(p.range), ao::TAN_HALF_FOV, w / h, if p.debug_reflectivity { 1.0 } else { 0.0 }],
        [1.0 / w, 1.0 / h, w, h],
        [FRESNEL_FLOOR, RAY_LENGTH, SKY_WEIGHT, MAX_LINEAR],
        [hw as f32, hh as f32, (p.classes & CLASSES_ALL) as f32, clamp_smart(p.smart)],
        [clamp_unit(p.hue_filter), clamp_unit(p.flatness), clamp_water(p.water_boost), clamp_depth_look(p.water_depth_look)],
        [clamp_matte(p.matte_cut), STATS_W as f32, STATS_H as f32, 0.0],
    ]
}

// ---- shaders (compiled once, in the background) -----------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub trace: Vec<u8>,
    pub blur_h: Vec<u8>,
    pub blur_v: Vec<u8>,
    pub mean: Vec<u8>,
    pub stats: Vec<u8>,
}

pub const PS_MEAN: &str = "ps_mean";
pub const PS_STATS: &str = "ps_stats";

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, trace: c(PS_TRACE, "ps_5_0")?, blur_h: c(PS_BLUR_H, "ps_5_0")?, blur_v: c(PS_BLUR_V, "ps_5_0")?, mean: c(PS_MEAN, "ps_5_0")?, stats: c(PS_STATS, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Sets the live values from the config and starts the background compile.
pub fn init(strength: f32, range: f32) {
    set_strength(strength);
    set_range(range);
    log!("ssr: configured (strength {}, range {})", self::strength(), self::range());
    let _ = std::thread::Builder::new().name("gw2g-ssr-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("ssr: shaders compiled"),
            Err(e) => log!("ssr: shader compile failed, reflections stay off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- the device objects ----------------------------------------------------------------------------

/// Device objects of the reflections, valid for one device only.
pub struct Ssr {
    device: usize,
    vs: Obj,
    ps_trace: Obj,
    ps_blur_h: Obj,
    ps_blur_v: Obj,
    cb: Obj,
    /// `b7` of the light-adjust substitute: one float4, x = strength.
    cb_sp: Obj,
    /// Half-size RGBA16F targets; 0 holds the finished reflection, 1 is the blur scratch.
    tex: [Obj; 2],
    srv: [Obj; 2],
    rtv: [Obj; 2],
    /// Half-size R16G16F water map written by the trace (x = water amount, y = ripple), read by the blurs as t2.
    tex_m: Obj,
    srv_m: Obj,
    rtv_m: Obj,
    size: (u32, u32),
    depth_srv: Obj,
    depth_key: usize,
    /// Matte gate: the frame-mean pass (1x1 RGBA32F, `t3` of the trace) and the telemetry grid (STATS_W x STATS_H
    /// RGBA32F) with its staging copy, read back without waiting a few frames later.
    ps_mean: Obj,
    ps_stats: Obj,
    tex_mean: Obj,
    srv_mean: Obj,
    rtv_mean: Obj,
    tex_st: Obj,
    rtv_st: Obj,
    staging: Obj,
    /// A copy into `staging` is queued and not read yet; the cut it was drawn with.
    pending: Option<f32>,
}

// The pointers are only used under the SSR mutex, from the render thread.
unsafe impl Send for Ssr {}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

impl Ssr {
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Ssr> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut a = Ssr { device: device as usize, vs: nul, ps_trace: nul, ps_blur_h: nul, ps_blur_v: nul, cb: nul, cb_sp: nul, tex: [nul; 2], srv: [nul; 2], rtv: [nul; 2], tex_m: nul, srv_m: nul, rtv_m: nul, size: (0, 0), depth_srv: nul, depth_key: 0, ps_mean: nul, ps_stats: nul, tex_mean: nul, srv_mean: nul, rtv_mean: nul, tex_st: nul, rtv_st: nul, staging: nul, pending: None };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut a.vs) >= 0
                && create_ps(device, b.trace.as_ptr(), b.trace.len(), nul, &mut a.ps_trace) >= 0
                && create_ps(device, b.blur_h.as_ptr(), b.blur_h.len(), nul, &mut a.ps_blur_h) >= 0
                && create_ps(device, b.blur_v.as_ptr(), b.blur_v.len(), nul, &mut a.ps_blur_v) >= 0
                && create_ps(device, b.mean.as_ptr(), b.mean.len(), nul, &mut a.ps_mean) >= 0
                && create_ps(device, b.stats.as_ptr(), b.stats.len(), nul, &mut a.ps_stats) >= 0;
            if !ok {
                log!("ssr: could not create the shaders");
                return None;
            }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let desc: [u32; 6] = [96, 2, 4, 0x10000, 0, 0];
            let desc_sp: [u32; 6] = [16, 2, 4, 0x10000, 0, 0];
            if create_buffer(device, &desc, std::ptr::null(), &mut a.cb) < 0 || create_buffer(device, &desc_sp, std::ptr::null(), &mut a.cb_sp) < 0 {
                log!("ssr: could not create the constant buffers");
                return None;
            }
            // The 1x1 frame mean and the telemetry grid (fixed sizes) and the staging copy of the grid.
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_TEXTURE2D));
            let create_srv: CreateViewFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_SRV));
            let create_rtv: CreateViewFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_RTV));
            let one = Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: FMT_RGBA32F, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
            let grid = Tex2dDesc { width: STATS_W, height: STATS_H, bind: 0x20, ..one };
            let stage = Tex2dDesc { usage: 3, bind: 0, cpu: 0x20000, ..grid };
            let sdesc: [u32; 6] = [FMT_RGBA32F, 4, 0, 1, 0, 0];
            let rdesc: [u32; 6] = [FMT_RGBA32F, 4, 0, 0, 0, 0];
            let ok = create_tex(device, &one, std::ptr::null(), &mut a.tex_mean) >= 0
                && !a.tex_mean.is_null()
                && create_srv(device, a.tex_mean, &sdesc, &mut a.srv_mean) >= 0
                && create_rtv(device, a.tex_mean, &rdesc, &mut a.rtv_mean) >= 0
                && create_tex(device, &grid, std::ptr::null(), &mut a.tex_st) >= 0
                && !a.tex_st.is_null()
                && create_rtv(device, a.tex_st, &rdesc, &mut a.rtv_st) >= 0
                && create_tex(device, &stage, std::ptr::null(), &mut a.staging) >= 0
                && !a.staging.is_null();
            if !ok {
                log!("ssr: could not create the matte-gate targets");
                return None;
            }
        }
        Some(a)
    }

    /// Reads the queued telemetry copy if the GPU is done with it (never waits unless `wait`). Some = a fresh summary.
    ///
    /// # Safety
    /// `ctx` must be the immediate context of the device this was created on.
    pub unsafe fn poll_stats(&mut self, ctx: Obj, wait: bool) -> Option<MatteStats> {
        let cut = self.pending?;
        let _ig = IgnoreScope::new();
        unsafe {
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let mut m: [usize; 3] = [0; 3];
            // READ (1); D3D11_MAP_FLAG_DO_NOT_WAIT (0x100000): DXGI_ERROR_WAS_STILL_DRAWING while the copy is in flight.
            if map(ctx, self.staging, 0, 1, if wait { 0 } else { 0x100000 }, &mut m) < 0 || m[0] == 0 {
                return None;
            }
            // D3D11_MAPPED_SUBRESOURCE on x64 is { void* pData; UINT RowPitch; UINT DepthPitch }: the row pitch is the low
            // 32 bits of the second word.
            let pitch = m[1] & 0xffff_ffff;
            let mut v = Vec::with_capacity((STATS_W * STATS_H) as usize);
            for y in 0..STATS_H as usize {
                let row = std::slice::from_raw_parts((m[0] + y * pitch) as *const [f32; 4], STATS_W as usize);
                v.extend_from_slice(row);
            }
            unmap(ctx, self.staging, 0);
            self.pending = None;
            Some(summarize(&v, cut))
        }
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            for i in 0..2 {
                release(self.rtv[i]);
                release(self.srv[i]);
                release(self.tex[i]);
                self.rtv[i] = std::ptr::null_mut();
                self.srv[i] = std::ptr::null_mut();
                self.tex[i] = std::ptr::null_mut();
            }
            release(self.rtv_m);
            release(self.srv_m);
            release(self.tex_m);
            self.rtv_m = std::ptr::null_mut();
            self.srv_m = std::ptr::null_mut();
            self.tex_m = std::ptr::null_mut();
        }
        self.size = (0, 0);
    }

    /// (Re)creates the two half-size targets when the scene size changed.
    unsafe fn ensure_targets(&mut self, dev: Obj, full: (u32, u32)) -> bool {
        let size = half_size(full.0, full.1);
        if self.size == size && !self.tex[0].is_null() {
            return true;
        }
        unsafe {
            self.free_targets();
            let desc = Tex2dDesc { width: size.0, height: size.1, mips: 1, array: 1, format: FMT_R16G16B16A16_FLOAT, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let create_rtv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_RTV));
            let sdesc: [u32; 6] = [FMT_R16G16B16A16_FLOAT, 4, 0, 1, 0, 0];
            let rdesc: [u32; 6] = [FMT_R16G16B16A16_FLOAT, 4, 0, 0, 0, 0];
            for i in 0..2 {
                if create_tex(dev, &desc, std::ptr::null(), &mut self.tex[i]) < 0
                    || self.tex[i].is_null()
                    || create_srv(dev, self.tex[i], &sdesc, &mut self.srv[i]) < 0
                    || create_rtv(dev, self.tex[i], &rdesc, &mut self.rtv[i]) < 0
                {
                    self.free_targets();
                    return false;
                }
            }
            let mdesc = Tex2dDesc { format: FMT_R16G16_FLOAT, ..desc };
            let msdesc: [u32; 6] = [FMT_R16G16_FLOAT, 4, 0, 1, 0, 0];
            let mrdesc: [u32; 6] = [FMT_R16G16_FLOAT, 4, 0, 0, 0, 0];
            if create_tex(dev, &mdesc, std::ptr::null(), &mut self.tex_m) < 0
                || self.tex_m.is_null()
                || create_srv(dev, self.tex_m, &msdesc, &mut self.srv_m) < 0
                || create_rtv(dev, self.tex_m, &mrdesc, &mut self.rtv_m) < 0
            {
                self.free_targets();
                return false;
            }
        }
        self.size = size;
        log!("ssr: reflection targets {}x{} (scene {}x{})", size.0, size.1, full.0, full.1);
        true
    }

    unsafe fn ensure_depth_srv(&mut self, dev: Obj, depth: Obj) -> bool {
        if !self.depth_srv.is_null() && self.depth_key == depth as usize {
            return true;
        }
        unsafe {
            release(self.depth_srv);
            self.depth_srv = std::ptr::null_mut();
            self.depth_key = 0;
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let sdesc: [u32; 6] = [crate::passes::FMT_R32_FLOAT, 4, 0, 1, 0, 0];
            if create_srv(dev, depth, &sdesc, &mut self.depth_srv) < 0 || self.depth_srv.is_null() {
                self.depth_srv = std::ptr::null_mut();
                return false;
            }
        }
        self.depth_key = depth as usize;
        true
    }

    /// SRV of the finished reflection (valid after `run`, until the targets are recreated).
    pub fn result_srv(&self) -> Obj {
        self.srv[0]
    }

    /// Runs trace + two blurs. `scene_srv` is a live SRV of the full-size scene colour, `depth` the R32_FLOAT
    /// linear depth of the same size. All pipeline state touched is restored. False when setup failed.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, depth: Obj, scene_srv: Obj, p: &SsrParams, strength: f32) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_targets(dev, (p.width, p.height)) || !self.ensure_depth_srv(dev, depth) {
                return false;
            }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [[f32; 4]; 6]) = cb_values(p);
            unmap(ctx, self.cb, 0);
            if map(ctx, self.cb_sp, 0, 4, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [f32; 4]) = [clamp_strength(strength), 0.0, 0.0, 0.0];
            unmap(ctx, self.cb_sp, 0);

            let _state = StateGuard::save(ctx);
            let (hw, hh) = self.size;
            let vp = Viewport { x: 0.0, y: 0.0, w: hw as f32, h: hh as f32, min_depth: 0.0, max_depth: 1.0 };
            let factor = [1.0f32; 4];
            let set_rt: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
            let set_vp: unsafe extern "system" fn(Obj, u32, *const Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_VIEWPORTS));
            let set_rs: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_STATE));
            let set_blend: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
            let set_depth: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
            let set_layout: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_LAYOUT));
            let set_topology: unsafe extern "system" fn(Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_TOPOLOGY));
            type SetShader = unsafe extern "system" fn(Obj, Obj, *const Obj, u32);
            let set_vs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_SET_SHADER));
            let set_gs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_SET_SHADER));
            let set_ps: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            let draw: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));

            set_vp(ctx, 1, &vp);
            set_rs(ctx, std::ptr::null_mut());
            set_depth(ctx, std::ptr::null_mut(), 0);
            set_layout(ctx, std::ptr::null_mut());
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, std::ptr::null_mut(), std::ptr::null(), 0);
            set_blend(ctx, std::ptr::null_mut(), factor.as_ptr(), 0xffff_ffff);
            set_cb(ctx, 0, 1, &self.cb);

            // 0: the frame's mean colour (1x1) for the matte gate's white balance (t3 of the trace and of the stats).
            let nul: Obj = std::ptr::null_mut();
            let vp1 = Viewport { x: 0.0, y: 0.0, w: 1.0, h: 1.0, min_depth: 0.0, max_depth: 1.0 };
            set_vp(ctx, 1, &vp1);
            set_rt(ctx, 1, &self.rtv_mean, nul);
            set_ps(ctx, self.ps_mean, std::ptr::null(), 0);
            set_srv(ctx, 0, 4, [self.depth_srv, scene_srv, nul, nul].as_ptr());
            draw(ctx, 3, 0);
            // Telemetry grid (only when asked and no earlier copy is still unread).
            if p.stats && self.pending.is_none() {
                let vps = Viewport { x: 0.0, y: 0.0, w: STATS_W as f32, h: STATS_H as f32, min_depth: 0.0, max_depth: 1.0 };
                set_vp(ctx, 1, &vps);
                set_rt(ctx, 1, &self.rtv_st, nul);
                set_ps(ctx, self.ps_stats, std::ptr::null(), 0);
                set_srv(ctx, 0, 4, [self.depth_srv, scene_srv, nul, self.srv_mean].as_ptr());
                draw(ctx, 3, 0);
                let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
                set_rt(ctx, 0, std::ptr::null(), nul);
                copy(ctx, self.staging, self.tex_st);
                self.pending = Some(clamp_matte(p.matte_cut));
            }
            set_vp(ctx, 1, &vp);
            // 1: trace into 0.
            // The second target is the water map (blur width per pixel).
            set_rt(ctx, 2, [self.rtv[0], self.rtv_m].as_ptr(), std::ptr::null_mut());
            set_ps(ctx, self.ps_trace, std::ptr::null(), 0);
            set_srv(ctx, 0, 4, [self.depth_srv, scene_srv, nul, self.srv_mean].as_ptr());
            draw(ctx, 3, 0);
            // 2: horizontal blur 0 -> 1.
            set_rt(ctx, 1, &self.rtv[1], std::ptr::null_mut());
            set_ps(ctx, self.ps_blur_h, std::ptr::null(), 0);
            set_srv(ctx, 0, 3, [self.depth_srv, self.srv[0], self.srv_m].as_ptr());
            draw(ctx, 3, 0);
            // 3: vertical blur 1 -> 0 (the finished reflection).
            set_rt(ctx, 1, &self.rtv[0], std::ptr::null_mut());
            set_ps(ctx, self.ps_blur_v, std::ptr::null(), 0);
            set_srv(ctx, 0, 3, [self.depth_srv, self.srv[1], self.srv_m].as_ptr());
            draw(ctx, 3, 0);
            // StateGuard restores slots 0 and 1 only: leave nothing of ours in slots 2 and 3.
            set_srv(ctx, 2, 2, [nul, nul].as_ptr());
        }
        true
    }
}

impl Drop for Ssr {
    fn drop(&mut self) {
        unsafe {
            self.free_targets();
            for o in [self.staging, self.rtv_st, self.tex_st, self.rtv_mean, self.srv_mean, self.tex_mean, self.ps_stats, self.ps_mean, self.depth_srv, self.cb_sp, self.cb, self.ps_blur_v, self.ps_blur_h, self.ps_trace, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from datapath::bind_current) ---------------------------------------------

static SSR: Mutex<Option<Ssr>> = Mutex::new(None);

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// Binds `t8` / `b7` of the light-adjust substitute (null = unbound = feature off).
unsafe fn bind_slots(ctx: Obj, srv: Obj, cb: Obj) {
    unsafe {
        type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
        let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
        set_srv(ctx, SLOT_T, 1, &srv);
        set_cb(ctx, SLOT_B, 1, &cb);
    }
    BOUND.store(!srv.is_null(), Relaxed);
}

/// The light-adjust draw is about to be issued on `ctx` (device `dev`): computes the reflection texture once per
/// frame and binds it (`t8`, `b7`). When it cannot or must not run, the two slots are left unbound (zero = off).
/// Never unwinds.
///
/// # Safety
/// `ctx` and `dev` must be live objects of the same device.
pub unsafe fn on_light_adjust(ctx: Obj, dev: Obj) {
    let r = std::panic::catch_unwind(|| unsafe { on_light_adjust_inner(ctx, dev) });
    if r.is_err() {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        log_once(&LOGGED, || "ssr: panic caught in the light-adjust glue; reflections off for this frame".to_string());
    }
}

unsafe fn on_light_adjust_inner(ctx: Obj, dev: Obj) {
    let nul: Obj = std::ptr::null_mut();
    unsafe {
        if strength() <= 0.0 || !ao::enabled() || FAILED.load(Relaxed) {
            if BOUND.load(Relaxed) {
                bind_slots(ctx, nul, nul);
            }
            return;
        }
        let Ok(mut guard) = SSR.try_lock() else { return };
        if DONE.load(Relaxed) {
            // A later draw with the same shader in the same frame: just keep the slots bound.
            if let Some(s) = guard.as_ref().filter(|s| s.device == dev as usize) {
                bind_slots(ctx, s.result_srv(), s.cb_sp);
            }
            return;
        }
        let ran = run_frame(&mut guard, ctx, dev);
        match (ran, guard.as_ref()) {
            (true, Some(s)) => {
                bind_slots(ctx, s.result_srv(), s.cb_sp);
                DONE.store(true, Relaxed);
                CUR.fetch_or(SEEN_APPLIED, Relaxed);
            }
            _ => {
                if BOUND.load(Relaxed) {
                    bind_slots(ctx, nul, nul);
                }
            }
        }
    }
}

/// Gathers the inputs and runs the passes. Every refusal is logged once and noted for the overlay.
unsafe fn run_frame(guard: &mut Option<Ssr>, ctx: Obj, dev: Obj) -> bool {
    static NO_DEPTH: AtomicBool = AtomicBool::new(false);
    static NO_SCENE: AtomicBool = AtomicBool::new(false);
    static BAD_SIZE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    unsafe {
        let blobs = match BLOBS.get() {
            None => return false, // still compiling
            Some(Err(_)) => {
                FAILED.store(true, Relaxed);
                return false;
            }
            Some(Ok(b)) => b,
        };
        let Some(depth) = ao::depth_resource() else {
            CUR.fetch_or(SEEN_NO_DEPTH, Relaxed);
            log_once(&NO_DEPTH, || "ssr: skipped, no linear depth for this frame (depth consumer registered but the depth pass was not found)".to_string());
            return false;
        };
        // The scene colour: PS SRV slot 0 right now (a reference we release).
        let mut scene_srv: Obj = std::ptr::null_mut();
        let get_srvs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        get_srvs(ctx, 0, 1, &mut scene_srv);
        if scene_srv.is_null() {
            CUR.fetch_or(SEEN_NO_SCENE, Relaxed);
            log_once(&NO_SCENE, || "ssr: skipped, no scene colour bound at PS slot 0 of the light-adjust draw".to_string());
            return false;
        }
        let scene = crate::ao::texture_of(scene_srv);
        let (scene_size, scene_ok) = match &scene {
            Some((_, d)) => ((d.width, d.height), d.samples == 1 && d.mips >= 1),
            None => ((0, 0), false),
        };
        if let Some((res, _)) = scene {
            release(res);
        }
        let mut dd = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(depth, vt::TEX2D_GET_DESC));
        get_desc(depth, &mut dd);
        if !scene_ok || scene_size != (dd.width, dd.height) || dd.width == 0 || dd.height == 0 {
            release(scene_srv);
            CUR.fetch_or(SEEN_BAD_SIZE, Relaxed);
            log_once(&BAD_SIZE, || format!("ssr: skipped, scene colour {}x{} (single sample: {scene_ok}) does not match the depth {}x{}", scene_size.0, scene_size.1, dd.width, dd.height));
            return false;
        }
        if guard.as_ref().is_some_and(|s| s.device != dev as usize) {
            *guard = None;
        }
        if guard.is_none() {
            match Ssr::new(dev, blobs) {
                Some(s) => *guard = Some(s),
                None => {
                    release(scene_srv);
                    FAILED.store(true, Relaxed);
                    log!("ssr: could not be created; reflections stay off");
                    return false;
                }
            }
        }
        let s = guard.as_mut().expect("just created");
        // Telemetry: read the copy queued earlier if the GPU is done with it (never waits), and queue a new one now and then.
        if let Some(st) = s.poll_stats(ctx, false) {
            if let Ok(mut g) = STATS.lock() {
                *g = Some(st);
            }
        }
        let stats_due = FRAMES.fetch_add(1, Relaxed) % STATS_EVERY == 0;
        let p = SsrParams { range: range(), width: dd.width, height: dd.height, classes: classes(), smart: smart(), hue_filter: hue_filter(), flatness: flatness(), debug_reflectivity: crate::resolve::debug_level() == 5, water_boost: water_boost(), water_depth_look: depth_look(), matte_cut: matte_cut(), stats: stats_due };
        let ok = s.run(dev, ctx, depth, scene_srv, &p, strength());
        release(scene_srv);
        if !ok {
            CUR.fetch_or(SEEN_NO_RUN, Relaxed);
            log_once(&NO_RUN, || "ssr: the targets, the depth view or the constants could not be set up; frame left without reflections".to_string());
            return false;
        }
        log_once(&FIRST, || format!("ssr: first frame applied (strength {}, range {}, matte cut {}, scene {}x{}, targets {}x{})", strength(), range(), matte_cut(), dd.width, dd.height, half_size(dd.width, dd.height).0, half_size(dd.width, dd.height).1));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(clamp_strength(9.0), 2.0);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), 0.5);
        assert_eq!(clamp_range(0.0), 0.25);
        assert_eq!(clamp_range(100.0), 2.0);
        assert_eq!(clamp_range(f32::NAN), 1.0);
        assert_eq!(clamp_range(f32::INFINITY), 2.0);
    }

    #[test]
    fn constants_follow_the_params() {
        let c = cb_values(&SsrParams { range: 1.5, width: 3440, height: 1440, classes: 5, smart: 0.5, hue_filter: 0.25, flatness: 0.75, debug_reflectivity: true, water_boost: 0.5, water_depth_look: 0.25, matte_cut: 0.3, stats: false });
        assert_eq!(c[0][0], 1.5);
        assert_eq!(c[0][1], ao::TAN_HALF_FOV);
        assert!((c[0][2] - 3440.0 / 1440.0).abs() < 1e-6);
        assert_eq!((c[1][2], c[1][3]), (3440.0, 1440.0));
        assert_eq!((c[3][0], c[3][1]), (1720.0, 720.0));
        assert_eq!((c[3][2], c[3][3]), (5.0, 0.5));
        assert_eq!((c[4][0], c[4][1], c[0][3]), (0.25, 0.75, 1.0));
        assert_eq!((c[4][2], c[4][3]), (0.5, 0.25));
        assert_eq!(c[5], [0.3, STATS_W as f32, STATS_H as f32, 0.0]);
        let c = cb_values(&SsrParams { range: 9.0, width: 0, height: 0, classes: 0xff, smart: 9.0, hue_filter: 9.0, flatness: -1.0, debug_reflectivity: false, water_boost: 9.0, water_depth_look: f32::NAN, matte_cut: f32::NAN, stats: true });
        assert_eq!(c[0][0], 2.0);
        assert_eq!((c[3][2], c[3][3]), (7.0, 1.0));
        assert_eq!((c[4][0], c[4][1], c[0][3]), (1.0, 0.0, 0.0));
        assert_eq!((c[4][2], c[4][3]), (1.0, DEFAULT_DEPTH_LOOK), "boost clamps to 1, NaN depth look gives the default");
        assert_eq!(c[5][0], DEFAULT_MATTE, "NaN matte cut gives the default");
        assert_eq!(half_size(5, 3), (3, 2));
        assert_eq!(half_size(0, 0), (1, 1));
    }

    #[test]
    fn material_settings_clamp() {
        assert_eq!(clamp_smart(3.0), 1.0);
        assert_eq!(clamp_smart(-3.0), 0.0);
        assert_eq!(clamp_smart(f32::NAN), 1.0);
        assert_eq!(clamp_smart(0.3), 0.3);
        // Only this test touches SMART and CLASSES among the host tests.
        assert_eq!(set_classes(0xff), 7);
        assert_eq!(classes(), 7);
        assert_eq!(set_classes(5), 5);
        assert_eq!(set_smart(0.25), 0.25);
        assert_eq!(smart(), 0.25);
        set_classes(CLASSES_ALL);
        set_smart(DEFAULT_SMART);
    }

    #[test]
    fn water_settings_clamp_default_and_roundtrip() {
        assert_eq!((DEFAULT_WATER, DEFAULT_DEPTH_LOOK), (0.6, 0.3));
        assert_eq!(f32::from_bits(0x3f19_999a), DEFAULT_WATER);
        assert_eq!(f32::from_bits(0x3e99_999a), DEFAULT_DEPTH_LOOK);
        assert_eq!((clamp_water(3.0), clamp_water(-1.0), clamp_water(f32::NAN)), (1.0, 0.0, 0.6));
        assert_eq!((clamp_depth_look(3.0), clamp_depth_look(-1.0), clamp_depth_look(f32::NAN)), (1.0, 0.0, 0.3));
        // Only this test touches WATER and DEPTH_LOOK among the host tests.
        let (kw, kd) = (water_boost(), depth_look());
        assert_eq!(set_water_boost(0.25), 0.25);
        assert_eq!(water_boost(), 0.25);
        assert_eq!(set_depth_look(0.5), 0.5);
        assert_eq!(depth_look(), 0.5);
        set_water_boost(kw);
        set_depth_look(kd);
    }

    #[test]
    fn matte_cut_clamps_defaults_and_roundtrips() {
        assert_eq!(DEFAULT_MATTE, 0.8);
        assert_eq!(f32::from_bits(0x3f4c_cccd), DEFAULT_MATTE);
        assert_eq!((clamp_matte(3.0), clamp_matte(-1.0), clamp_matte(f32::NAN), clamp_matte(0.35)), (1.0, 0.0, 0.8, 0.35));
        assert_eq!(clamp_matte(f32::INFINITY), 1.0);
        // Only this test touches MATTE among the host tests.
        let keep = matte_cut();
        assert_eq!(set_matte_cut(0.25), 0.25);
        assert_eq!(matte_cut(), 0.25);
        assert_eq!(set_matte_cut(f32::NAN), DEFAULT_MATTE);
        set_matte_cut(keep);
    }

    #[test]
    fn telemetry_counts_floors_classes_and_rejections() {
        // x floor, y score with the cut, z without, w water part.
        let s = summarize(&[
            [0.0, 1.0, 1.0, 1.0],      // not a floor: ignored
            [1.0, 0.0, 0.0, 0.0],      // floor, never reflective
            [1.0, 0.9, 0.9, 1.0],      // water, kept
            [1.0, 0.0, 0.8, 0.0],      // dry dirt: rejected by the gate
            [1.0, 0.0, 0.6, 0.0],      // rejected
            [1.0, 0.5, 0.5, 0.1],      // other, kept
            [1.0, f32::NAN, f32::NAN, f32::NAN], // NaN counts as nothing
        ], 0.8);
        assert_eq!((s.samples, s.floors, s.reflective_water, s.reflective_other, s.would_reflect, s.rejected), (7, 6, 1, 1, 4, 2));
        let t = stats_text(&s);
        assert!(t.contains("matte cut 0.80") && t.contains("reflective 2 (water/glass 1, other 1)") && t.contains("rejected 2 = 50 %"), "{t}");
        let e = stats_text(&summarize(&[], 0.0));
        assert!(e.contains("floors 0.0 %") && !e.contains("NaN"), "{e}");
    }

    #[test]
    fn strength_drives_the_depth_consumer() {
        // Only this test touches STRENGTH and the SSR depth-consumer bit among the host tests.
        let keep = strength();
        set_strength(0.0);
        assert_eq!(strength(), 0.0);
        assert!(!wants());
        set_strength(0.7);
        assert_eq!(strength(), 0.7);
        set_strength(f32::NAN);
        assert_eq!(strength(), DEFAULT_STRENGTH);
        set_strength(keep);
        ao::set_depth_consumer(ao::DEPTH_FOR_SSR, false);
    }
}

#[cfg(test)]
#[path = "ssr_tests.rs"]
mod gpu_tests;
