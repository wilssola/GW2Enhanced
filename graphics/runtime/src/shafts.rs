//! Volumetric light as screen-space sun shafts (god rays), milestone M3. See `docs/shafts.md`.
//!
//! The engine's shadow matrices and sun direction are not known, so everything is estimated from the picture:
//! the SKY (pixels with invalid linear depth) that is bright is the emitter, geometry is the occluder, and only a
//! compact core that clearly stands out from the rest of the sky is taken as the sun. At the
//! light-adjust draw (`datapath::bind_current` calls `on_light_adjust`) a few small draws run in this module's
//! own targets (the AO pattern, see `data-path.md`: nothing is drawn into the engine's targets):
//!
//! 0. depth statistics: 16x16 tiles of (max valid depth, invalid count), then the 1x1 frame maximum and invalid
//!    fraction. They make the sky test robust: a tap is sky when its depth is invalid, or, when fewer than 2 % of
//!    the depth samples are invalid (the sky is NOT encoded as invalid depth), when it is within 2 % of the
//!    frame's farthest depth (a finite far plane). The rule is off when many samples are invalid, because a flat
//!    scene with an invalid-depth sky would otherwise classify its ground as sky,
//! 1. emitter mask at 1/4 size (bright sky colour; the scene colour is the pixel shader SRV in slot 0),
//! 2. reduction chain: 16x16 tile sums (with the tile peak), 16x16 tiles of the sun core (the pixels in the upper
//!    half between the mean sky luminance and the frame's peak), then the 1x1 sun state (position = core
//!    centroid, confidence, colour tint) and the 1x1 measurement (peak, sky background, emitter radius). The 1x1
//!    state is blended with the previous one in a ping-pong pair of textures, entirely on the GPU: no CPU
//!    read-back in the frame path, so no stall (the overlay's status line reads an old copy through a ring of
//!    staging textures with `DO_NOT_WAIT`, which never blocks either),
//! 3. radial blur towards the sun with per-pixel jitter (GPU Gems 3, chapter 13), in which only the emitter
//!    within the emitter radius of the sun emits: the rest of the bright sky does not.
//!
//! The result (RGBA16F, rgb = shaft light in linear units, already tinted) is bound as `t9` of the light-adjust
//! substitute, with `b8.x` = strength; the shader adds it in linear light, weaker on pixels that are already
//! bright. Unbound `t9`/`b8` read zero, which means off. Strength 0 runs nothing and releases the depth consumer.
//!
//! The shaders are compiled once on a background thread (never `D3DCompile` on the render thread); until that
//! has finished the feature is skipped. Live settings are atomics copied into constant buffers on every frame.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::log;
use crate::{ao, shaders};

pub const HLSL: &str = include_str!("../shaders/shafts.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_DTILE: &str = "ps_dtile";
pub const PS_DREDUCE: &str = "ps_dreduce";
pub const PS_MASK: &str = "ps_mask";
pub const PS_REDUCE_A: &str = "ps_reduce_a";
pub const PS_CORE: &str = "ps_core";
pub const PS_REDUCE_B: &str = "ps_reduce_b";
pub const PS_SHAFTS: &str = "ps_shafts";

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 2.0);
pub const LENGTH_RANGE: (f32, f32) = (0.3, 1.0);
pub const DEFAULT_STRENGTH: f32 = 0.6;
pub const DEFAULT_LENGTH: f32 = 0.7;

/// Per-sample decay of the ray (applied 56 times: 0.965^56 = 0.14).
pub const DECAY: f32 = 0.965;
/// Per-frame blend of the confidence towards a higher / lower measurement.
pub const CONF_RISE: f32 = 0.2;
pub const CONF_FALL: f32 = 0.1;
/// Per-frame blend of the sun position and tint towards the measurement.
pub const FOLLOW: f32 = 0.25;
/// A tap is far-plane sky when its depth is at least this fraction of the frame's maximum valid depth ...
pub const FAR_RATIO: f32 = 0.98;
/// ... and the share of invalid depth samples is below this.
pub const MAX_INVALID_FOR_FAR: f32 = 0.02;
/// Size of the tile grid of the first reduction (the shader hard-codes 16).
pub const TILES: u32 = 16;

/// Slots of the light-adjust substitute.
pub const SRV_SLOT: u32 = 9;
pub const CB_SLOT: u32 = 8;

// Local vtable indices (d3d11.h, SDK 10.0.26100) that `d3d::vt` does not list.
const CTX_PS_SET_SAMPLERS: usize = 10;
const DEV_CREATE_SAMPLER_STATE: usize = 23;

const FMT_RGBA32F: u32 = 2;
const FMT_RGBA16F: u32 = 10;
const FMT_R32_FLOAT: u32 = 41;

// ---- switches ------------------------------------------------------------------------------------

static STRENGTH: AtomicU32 = AtomicU32::new(0x3f19_999a); // 0.6f32
static LENGTH: AtomicU32 = AtomicU32::new(0x3f33_3333); // 0.7f32
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
/// t9 / b8 are currently bound on the game's context (so they can be unbound when the feature stops).
static BOUND: AtomicBool = AtomicBool::new(false);
static CUR: AtomicU8 = AtomicU8::new(0);
static LAST: AtomicU8 = AtomicU8::new(0);
const SEEN_DRAW: u8 = 1;
const SEEN_APPLIED: u8 = 2;
const SEEN_NO_DEPTH: u8 = 4;
const SEEN_BAD_SCENE: u8 = 8;
/// Slots used besides t0: t10..t19 (cleared after every pass).
const SLOT_FIRST: u32 = 10;
const SLOT_COUNT: u32 = 10;
const SLOT_DSTAT: u32 = 17;
const SLOT_DTILE: u32 = 18;
const SLOT_MEAS: u32 = 19;
/// The last sun estimate read back from the GPU (a few frames old): u, v, confidence.
static SUN_U: AtomicU32 = AtomicU32::new(0);
static SUN_V: AtomicU32 = AtomicU32::new(0);
static SUN_CONF: AtomicU32 = AtomicU32::new(0);
/// Sky fraction and the frame's max valid depth of the same estimate.
static SKY_FRAC: AtomicU32 = AtomicU32::new(0);
static MAX_DEPTH: AtomicU32 = AtomicU32::new(0);
/// Milliseconds (since the first call) of the last debug log line.
static LAST_DEBUG_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Clamps to 0..=2; NaN gives the default.
pub fn clamp_strength(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_STRENGTH } else { v.clamp(STRENGTH_RANGE.0, STRENGTH_RANGE.1) }
}

/// Clamps to 0.3..=1; NaN gives the default.
pub fn clamp_length(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_LENGTH } else { v.clamp(LENGTH_RANGE.0, LENGTH_RANGE.1) }
}

/// Sets the strength (live). 0 turns the feature off completely: no passes, and the depth consumer is released.
pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    ao::set_depth_consumer(ao::DEPTH_FOR_SHAFTS, v > 0.0);
    v
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Shafts, f32::from_bits(STRENGTH.load(Relaxed)))
}

pub fn set_length(v: f32) -> f32 {
    let v = clamp_length(v);
    LENGTH.store(v.to_bits(), Relaxed);
    v
}

pub fn length() -> f32 {
    f32::from_bits(LENGTH.load(Relaxed))
}

/// A new frame starts: remembers what the last one did (overlay), allows the passes to run again.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
    LAST.store(CUR.swap(0, Relaxed), Relaxed);
}

/// The last sun estimate the GPU produced (u, v in 0..1, confidence 0..1), read back lazily; None before the first.
pub fn last_sun() -> Option<(f32, f32, f32)> {
    let c = f32::from_bits(SUN_CONF.load(Relaxed));
    (SUN_CONF.load(Relaxed) != 0 || SUN_U.load(Relaxed) != 0).then(|| (f32::from_bits(SUN_U.load(Relaxed)), f32::from_bits(SUN_V.load(Relaxed)), c))
}

/// (sky fraction, max valid depth) of the last read-back estimate.
pub fn last_depth_info() -> (f32, f32) {
    (f32::from_bits(SKY_FRAC.load(Relaxed)), f32::from_bits(MAX_DEPTH.load(Relaxed)))
}

/// Overlay line.
pub fn status_text() -> String {
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    if strength() <= 0.0 {
        return "off (strength 0)".to_string();
    }
    let l = LAST.load(Relaxed);
    if l & SEEN_APPLIED != 0 {
        return match last_sun() {
            Some((u, v, c)) if c >= 0.05 => format!("sun found at ({u:.2}, {v:.2}), confidence {c:.2}, sky {:.0} %, max depth {}", last_depth_info().0 * 100.0, fmt_depth(last_depth_info().1)),
            Some((_, _, c)) => format!("sun not found (no bright sky), confidence {c:.2}, sky {:.0} %, max depth {}", last_depth_info().0 * 100.0, fmt_depth(last_depth_info().1)),
            None => "applied, waiting for the first estimate".to_string(),
        };
    }
    if l & SEEN_NO_DEPTH != 0 {
        "skipped: depth not found".to_string()
    } else if l & SEEN_BAD_SCENE != 0 {
        "skipped: scene colour not usable (see log)".to_string()
    } else if BLOBS.get().is_none() {
        "compiling shaders".to_string()
    } else if l & SEEN_DRAW != 0 {
        "skipped: could not run (see log)".to_string()
    } else {
        "waiting for a scene frame".to_string()
    }
}

/// Compact depth text for the overlay and the log.
pub fn fmt_depth(d: f32) -> String {
    if !d.is_finite() { "n/a".to_string() } else if d >= 100.0 { format!("{d:.0}") } else { format!("{d:.1}") }
}

// ---- constants --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Scene size in pixels (the emitter is 1/4 of it).
    pub width: u32,
    pub height: u32,
    pub length: f32,
    /// Debug levels 1 and 4 draw a cross at the sun.
    pub debug: bool,
}

/// Size of the quarter-resolution targets for a scene of `w` x `h`.
pub fn quarter(w: u32, h: u32) -> (u32, u32) {
    ((w.max(1) + 3) / 4, (h.max(1) + 3) / 4)
}

/// The three constant-buffer vectors of `shafts.hlsl` (see its header).
pub fn cb_values(p: &Params) -> [[f32; 4]; 4] {
    let (qw, qh) = quarter(p.width, p.height);
    let (qw, qh) = (qw as f32, qh as f32);
    [[qw, qh, 1.0 / qw, 1.0 / qh], [clamp_length(p.length), DECAY, if p.debug { 1.0 } else { 0.0 }, 0.0], [qw * qh, CONF_RISE, CONF_FALL, FOLLOW], [FAR_RATIO, MAX_INVALID_FOR_FAR, 0.0, 0.0]]
}

/// The constant buffer the light-adjust shader reads as b8: x = strength.
pub fn hp_values(strength: f32) -> [f32; 4] {
    [clamp_strength(strength), 0.0, 0.0, 0.0]
}

// ---- shaders (compiled once, in the background) -----------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub dtile: Vec<u8>,
    pub dreduce: Vec<u8>,
    pub mask: Vec<u8>,
    pub reduce_a: Vec<u8>,
    pub core: Vec<u8>,
    pub reduce_b: Vec<u8>,
    pub shafts: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, dtile: c(PS_DTILE, "ps_5_0")?, dreduce: c(PS_DREDUCE, "ps_5_0")?, mask: c(PS_MASK, "ps_5_0")?, reduce_a: c(PS_REDUCE_A, "ps_5_0")?, core: c(PS_CORE, "ps_5_0")?, reduce_b: c(PS_REDUCE_B, "ps_5_0")?, shafts: c(PS_SHAFTS, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Sets the live values from the config, registers the depth consumer and starts the background compile.
pub fn init(strength: f32, length: f32) {
    set_strength(strength);
    set_length(length);
    log!("shafts: configured (strength {}, length {})", self::strength(), self::length());
    let _ = std::thread::Builder::new().name("gw2g-shafts-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("shafts: shaders compiled"),
            Err(e) => log!("shafts: shader compile failed, shafts stay off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- device objects ---------------------------------------------------------------------------------

#[repr(C)]
struct SubresourceData {
    mem: *const c_void,
    pitch: u32,
    slice: u32,
}

#[repr(C)]
struct SamplerDesc {
    filter: u32,
    u: u32,
    v: u32,
    w: u32,
    bias: f32,
    max_aniso: u32,
    cmp: u32,
    border: [f32; 4],
    min_lod: f32,
    max_lod: f32,
}

/// A texture with its shader-resource and render-target views.
#[derive(Clone, Copy)]
pub(crate) struct Target {
    pub(crate) tex: Obj,
    pub(crate) srv: Obj,
    pub(crate) rtv: Obj,
}

pub(crate) const NO_TARGET: Target = Target { tex: std::ptr::null_mut(), srv: std::ptr::null_mut(), rtv: std::ptr::null_mut() };

impl Target {
    pub(crate) unsafe fn free(&mut self) {
        unsafe {
            release(self.rtv);
            release(self.srv);
            release(self.tex);
        }
        *self = NO_TARGET;
    }
}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

/// Creates a render-target + shader-resource texture; `zero` fills a 1x1 RGBA32F with zeros.
pub(crate) unsafe fn make_target(dev: Obj, w: u32, h: u32, fmt: u32, zero: bool) -> Option<Target> {
    let mut t = NO_TARGET;
    unsafe {
        let desc = Tex2dDesc { width: w, height: h, mips: 1, array: 1, format: fmt, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
        let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
        let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
        let create_rtv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_RTV));
        let zeros = [0u8; 16];
        let sd = SubresourceData { mem: zeros.as_ptr().cast(), pitch: 16, slice: 0 };
        let init: *const c_void = if zero { (&sd as *const SubresourceData).cast() } else { std::ptr::null() };
        let sdesc: [u32; 6] = [fmt, 4, 0, 1, 0, 0];
        let rdesc: [u32; 6] = [fmt, 4, 0, 0, 0, 0];
        if create_tex(dev, &desc, init, &mut t.tex) < 0 || t.tex.is_null() || create_srv(dev, t.tex, &sdesc, &mut t.srv) < 0 || create_rtv(dev, t.tex, &rdesc, &mut t.rtv) < 0 {
            t.free();
            return None;
        }
    }
    Some(t)
}

/// Device objects of the shafts, valid for one device only.
pub struct Shafts {
    device: usize,
    vs: Obj,
    ps_dtile: Obj,
    ps_dreduce: Obj,
    ps_mask: Obj,
    ps_ra: Obj,
    ps_core: Obj,
    ps_rb: Obj,
    ps_blur: Obj,
    cb: Obj,
    cb_hp: Obj,
    sampler: Obj,
    /// Scene size the targets were made for.
    size: (u32, u32),
    emit: Target,
    /// Depth statistics: 16x16 tiles, then the 1x1 frame values (max valid depth, invalid fraction).
    dtile: Target,
    dstat: Target,
    /// 16x16 tiles: [0] colour sums (tint), [1] luminance sum, sky count, tile peak, [2] sun core sums.
    tiles: [Target; 3],
    /// The 1x1 sun state, two sets (ping-pong): [set][0] = (u, v, confidence, mean luminance), [set][1] = tint.
    sun: [[Target; 2]; 2],
    cur: usize,
    /// This frame's sun measurement (1x1): peak luminance, mean sky luminance, emitter radius, core spread.
    meas: Target,
    /// The finished shaft light (RGBA16F, quarter size).
    out: Target,
    /// Ring of 1x1 staging copies of the sun state, for the lazy read-back.
    stage: [Obj; 3],
    /// The same for the second sun-state texture (tint, max depth).
    stage1: [Obj; 3],
    staged: [bool; 3],
    runs: u32,
    /// The last sun state the lazy read-back delivered.
    last_read: Option<[f32; 4]>,
    /// SRV of the depth resource it was made for.
    depth_srv: Obj,
    depth_key: usize,
}

// The pointers are only used under the mutex, from the render thread.
unsafe impl Send for Shafts {}

impl Shafts {
    /// Creates the shaders, the constant buffers and the sampler on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Shafts> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut s = Shafts {
            device: device as usize,
            vs: nul,
            ps_dtile: nul,
            ps_dreduce: nul,
            ps_mask: nul,
            ps_ra: nul,
            ps_core: nul,
            ps_rb: nul,
            ps_blur: nul,
            cb: nul,
            cb_hp: nul,
            sampler: nul,
            size: (0, 0),
            emit: NO_TARGET,
            dtile: NO_TARGET,
            dstat: NO_TARGET,
            tiles: [NO_TARGET; 3],
            sun: [[NO_TARGET; 2]; 2],
            cur: 0,
            meas: NO_TARGET,
            out: NO_TARGET,
            stage: [nul; 3],
            stage1: [nul; 3],
            staged: [false; 3],
            runs: 0,
            last_read: None,
            depth_srv: nul,
            depth_key: 0,
        };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut s.vs) >= 0
                && create_ps(device, b.dtile.as_ptr(), b.dtile.len(), nul, &mut s.ps_dtile) >= 0
                && create_ps(device, b.dreduce.as_ptr(), b.dreduce.len(), nul, &mut s.ps_dreduce) >= 0
                && create_ps(device, b.mask.as_ptr(), b.mask.len(), nul, &mut s.ps_mask) >= 0
                && create_ps(device, b.reduce_a.as_ptr(), b.reduce_a.len(), nul, &mut s.ps_ra) >= 0
                && create_ps(device, b.core.as_ptr(), b.core.len(), nul, &mut s.ps_core) >= 0
                && create_ps(device, b.reduce_b.as_ptr(), b.reduce_b.len(), nul, &mut s.ps_rb) >= 0
                && create_ps(device, b.shafts.as_ptr(), b.shafts.len(), nul, &mut s.ps_blur) >= 0;
            if !ok {
                log!("shafts: could not create the shaders");
                return None; // Drop releases what exists
            }
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &[64, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut s.cb) < 0 || create_buffer(device, &[16, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut s.cb_hp) < 0 {
                log!("shafts: could not create the constant buffers");
                return None;
            }
            // Linear filtering between texels, clamped to the edge of the picture.
            let sd = SamplerDesc { filter: 0x14 /* MIN_MAG_LINEAR_MIP_POINT */, u: 3, v: 3, w: 3, bias: 0.0, max_aniso: 1, cmp: 1, border: [0.0; 4], min_lod: 0.0, max_lod: f32::MAX };
            let create_sampler: unsafe extern "system" fn(Obj, *const SamplerDesc, *mut Obj) -> i32 = std::mem::transmute(vfn(device, DEV_CREATE_SAMPLER_STATE));
            if create_sampler(device, &sd, &mut s.sampler) < 0 || s.sampler.is_null() {
                log!("shafts: could not create the sampler");
                return None;
            }
        }
        Some(s)
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            self.emit.free();
            self.dtile.free();
            self.dstat.free();
            self.meas.free();
            self.out.free();
            for t in self.tiles.iter_mut() {
                t.free();
            }
            for set in self.sun.iter_mut() {
                for t in set.iter_mut() {
                    t.free();
                }
            }
            for s in self.stage.iter_mut().chain(self.stage1.iter_mut()) {
                release(*s);
                *s = std::ptr::null_mut();
            }
        }
        self.staged = [false; 3];
        self.cur = 0;
        self.size = (0, 0);
    }

    /// (Re)creates every target when the scene size changed. The sun state starts at zero (confidence 0).
    unsafe fn ensure_targets(&mut self, dev: Obj, size: (u32, u32)) -> bool {
        if self.size == size && !self.emit.tex.is_null() {
            return true;
        }
        unsafe {
            self.free_targets();
            let (qw, qh) = quarter(size.0, size.1);
            let mut ok = true;
            match make_target(dev, qw, qh, FMT_RGBA16F, false) {
                Some(t) => self.emit = t,
                None => ok = false,
            }
            match make_target(dev, qw, qh, FMT_RGBA16F, false) {
                Some(t) => self.out = t,
                None => ok = false,
            }
            match make_target(dev, TILES, TILES, FMT_RGBA32F, false) {
                Some(t) => self.dtile = t,
                None => ok = false,
            }
            match make_target(dev, 1, 1, FMT_RGBA32F, false) {
                Some(t) => self.dstat = t,
                None => ok = false,
            }
            match make_target(dev, 1, 1, FMT_RGBA32F, true) {
                Some(t) => self.meas = t,
                None => ok = false,
            }
            for i in 0..3 {
                match make_target(dev, TILES, TILES, FMT_RGBA32F, false) {
                    Some(t) => self.tiles[i] = t,
                    None => ok = false,
                }
            }
            for set in 0..2 {
                for k in 0..2 {
                    match make_target(dev, 1, 1, FMT_RGBA32F, true) {
                        Some(t) => self.sun[set][k] = t,
                        None => ok = false,
                    }
                }
            }
            let sd = Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: FMT_RGBA32F, samples: 1, quality: 0, usage: 3 /* STAGING */, bind: 0, cpu: 0x20000 /* READ */, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            for i in 0..3 {
                if create_tex(dev, &sd, std::ptr::null(), &mut self.stage[i]) < 0 {
                    self.stage[i] = std::ptr::null_mut();
                }
                if create_tex(dev, &sd, std::ptr::null(), &mut self.stage1[i]) < 0 {
                    self.stage1[i] = std::ptr::null_mut();
                }
            }
            if !ok {
                self.free_targets();
                return false;
            }
        }
        self.size = size;
        log!("shafts: targets {}x{} (scene {}x{})", quarter(size.0, size.1).0, quarter(size.0, size.1).1, size.0, size.1);
        true
    }

    /// SRV of the depth resource, cached while the resource is the same.
    unsafe fn ensure_depth_srv(&mut self, dev: Obj, depth: Obj) -> bool {
        if !self.depth_srv.is_null() && self.depth_key == depth as usize {
            return true;
        }
        unsafe {
            release(self.depth_srv);
            self.depth_srv = std::ptr::null_mut();
            self.depth_key = 0;
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let sdesc: [u32; 6] = [FMT_R32_FLOAT, 4, 0, 1, 0, 0];
            if create_srv(dev, depth, &sdesc, &mut self.depth_srv) < 0 || self.depth_srv.is_null() {
                self.depth_srv = std::ptr::null_mut();
                return false;
            }
        }
        self.depth_key = depth as usize;
        true
    }

    /// SRV of the finished shaft light (valid after `run`, until the targets are recreated).
    #[allow(dead_code)] // used by the tests; production binds it through `bind`
    pub fn out_srv(&self) -> Obj {
        self.out.srv
    }

    /// Runs the draws from the scene colour `scene_srv` and the linear `depth`. All pipeline state touched is
    /// restored. False when something failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `scene_srv` a view of a
    /// `p.width` x `p.height` colour texture, `depth` an R32_FLOAT texture with a shader-resource bind.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, scene_srv: Obj, depth: Obj, p: &Params) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_targets(dev, (p.width, p.height)) || !self.ensure_depth_srv(dev, depth) {
                return false;
            }
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [[f32; 4]; 4]) = cb_values(p);
            unmap(ctx, self.cb, 0);

            let _state = StateGuard::save(ctx);
            let (qw, qh) = quarter(p.width, p.height);
            let vp = |w: u32, h: u32| Viewport { x: 0.0, y: 0.0, w: w as f32, h: h as f32, min_depth: 0.0, max_depth: 1.0 };
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
            let set_smp: SetSlots = std::mem::transmute(vfn(ctx, CTX_PS_SET_SAMPLERS));
            let draw: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));
            let nul: Obj = std::ptr::null_mut();
            // The slots this module uses besides t0: t10..t19.
            let clear = || set_srv(ctx, SLOT_FIRST, SLOT_COUNT, [nul; SLOT_COUNT as usize].as_ptr());

            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, factor.as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);
            set_smp(ctx, 15, 1, &self.sampler);

            let new = 1 - self.cur;
            // 0a: depth -> 16x16 tiles (max valid depth, invalid count).
            clear();
            set_rt(ctx, 1, &self.dtile.rtv, nul);
            set_vp(ctx, 1, &vp(TILES, TILES));
            set_ps(ctx, self.ps_dtile, std::ptr::null(), 0);
            set_srv(ctx, SLOT_FIRST, 1, &self.depth_srv);
            draw(ctx, 3, 0);
            // 0b: tiles -> 1x1 frame depth statistics.
            clear();
            set_rt(ctx, 1, &self.dstat.rtv, nul);
            set_vp(ctx, 1, &vp(1, 1));
            set_ps(ctx, self.ps_dreduce, std::ptr::null(), 0);
            set_srv(ctx, SLOT_DTILE, 1, &self.dtile.srv);
            draw(ctx, 3, 0);
            // 1: scene colour + depth + depth statistics -> emitter.
            clear();
            set_rt(ctx, 1, &self.emit.rtv, nul);
            set_vp(ctx, 1, &vp(qw, qh));
            set_ps(ctx, self.ps_mask, std::ptr::null(), 0);
            set_srv(ctx, 0, 1, &scene_srv);
            set_srv(ctx, SLOT_FIRST, 1, &self.depth_srv);
            set_srv(ctx, SLOT_DSTAT, 1, &self.dstat.srv);
            draw(ctx, 3, 0);
            // 2a: emitter -> tile sums (colour, luminance, sky, tile peak).
            clear();
            let rts = [self.tiles[0].rtv, self.tiles[1].rtv];
            set_rt(ctx, 2, rts.as_ptr(), nul);
            set_vp(ctx, 1, &vp(TILES, TILES));
            set_ps(ctx, self.ps_ra, std::ptr::null(), 0);
            set_srv(ctx, 11, 1, &self.emit.srv);
            draw(ctx, 3, 0);
            // 2b: emitter + frame peak and mean sky luminance -> sun core tiles.
            clear();
            set_rt(ctx, 1, &self.tiles[2].rtv, nul);
            set_ps(ctx, self.ps_core, std::ptr::null(), 0);
            set_srv(ctx, 11, 1, &self.emit.srv);
            set_srv(ctx, 13, 1, &self.tiles[1].srv);
            draw(ctx, 3, 0);
            // 3: tile sums + previous state -> new 1x1 sun state and this frame's measurement.
            clear();
            let rts = [self.sun[new][0].rtv, self.sun[new][1].rtv, self.meas.rtv];
            set_rt(ctx, 3, rts.as_ptr(), nul);
            set_vp(ctx, 1, &vp(1, 1));
            set_ps(ctx, self.ps_rb, std::ptr::null(), 0);
            set_srv(ctx, 12, 5, [self.tiles[0].srv, self.tiles[1].srv, self.tiles[2].srv, self.sun[self.cur][0].srv, self.sun[self.cur][1].srv].as_ptr());
            set_srv(ctx, SLOT_DSTAT, 1, &self.dstat.srv);
            draw(ctx, 3, 0);
            self.cur = new;
            // 4: radial blur of the emitter towards the sun.
            clear();
            set_rt(ctx, 1, &self.out.rtv, nul);
            set_vp(ctx, 1, &vp(qw, qh));
            set_ps(ctx, self.ps_blur, std::ptr::null(), 0);
            set_srv(ctx, 11, 1, &self.emit.srv);
            set_srv(ctx, 15, 2, [self.sun[new][0].srv, self.sun[new][1].srv].as_ptr());
            set_srv(ctx, SLOT_MEAS, 1, &self.meas.srv);
            draw(ctx, 3, 0);
            // Leave nothing of ours bound (the guard restores t0, t1 and b0).
            clear();
            set_smp(ctx, 15, 1, &nul);

            self.read_back(ctx);
        }
        true
    }

    /// Copies the sun state into the staging ring and reads the oldest copy if the GPU is done with it (never waits).
    unsafe fn read_back(&mut self, ctx: Obj) {
        unsafe {
            let i = self.runs as usize % 3;
            self.runs = self.runs.wrapping_add(1);
            if self.stage[i].is_null() {
                return;
            }
            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
            copy(ctx, self.stage[i], self.sun[self.cur][0].tex);
            if !self.stage1[i].is_null() {
                copy(ctx, self.stage1[i], self.sun[self.cur][1].tex);
            }
            self.staged[i] = true;
            let j = (i + 1) % 3;
            if !self.staged[j] || self.stage[j].is_null() {
                return;
            }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let mut mapped: [usize; 3] = [0; 3];
            // READ, DO_NOT_WAIT: fails with "still drawing" instead of stalling.
            if map(ctx, self.stage[j], 0, 1, 0x10_0000, &mut mapped) >= 0 && mapped[0] != 0 {
                let v = *(mapped[0] as *const [f32; 4]);
                unmap(ctx, self.stage[j], 0);
                if v.iter().all(|x| x.is_finite()) {
                    if !self.stage1[j].is_null() && map(ctx, self.stage1[j], 0, 1, 0x10_0000, &mut mapped) >= 0 && mapped[0] != 0 {
                        let t = *(mapped[0] as *const [f32; 4]);
                        unmap(ctx, self.stage1[j], 0);
                        if t[3].is_finite() {
                            MAX_DEPTH.store(t[3].to_bits(), Relaxed);
                        }
                    }
                    SKY_FRAC.store(v[3].to_bits(), Relaxed);
                    self.last_read = Some(v);
                    SUN_U.store(v[0].to_bits(), Relaxed);
                    SUN_V.store(v[1].to_bits(), Relaxed);
                    SUN_CONF.store(v[2].to_bits(), Relaxed);
                }
            }
        }
    }

    /// Writes the strength into b8 and binds `t9` + `b8` on `ctx`.
    ///
    /// # Safety
    /// `ctx` must be a live immediate context of this device.
    pub unsafe fn bind(&self, ctx: Obj, strength: f32) -> bool {
        unsafe {
            let mut mapped: [usize; 3] = [0; 3];
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            if map(ctx, self.cb_hp, 0, 4, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [f32; 4]) = hp_values(strength);
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            unmap(ctx, self.cb_hp, 0);
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            set_srv(ctx, SRV_SLOT, 1, &self.out.srv);
            set_cb(ctx, CB_SLOT, 1, &self.cb_hp);
        }
        true
    }
}

/// Unbinds `t9` and `b8` (they then read zero: no shafts).
///
/// # Safety
/// `ctx` must be a live device context.
pub unsafe fn unbind(ctx: Obj) {
    unsafe {
        type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
        let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
        let nul: Obj = std::ptr::null_mut();
        set_srv(ctx, SRV_SLOT, 1, &nul);
        set_cb(ctx, CB_SLOT, 1, &nul);
    }
}

impl Drop for Shafts {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            self.free_targets();
            for o in [self.depth_srv, self.sampler, self.cb_hp, self.cb, self.ps_blur, self.ps_rb, self.ps_core, self.ps_ra, self.ps_mask, self.ps_dreduce, self.ps_dtile, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from datapath::bind_current) ------------------------------------------------

static SHAFTS: Mutex<Option<Shafts>> = Mutex::new(None);

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// Debug level 4: the sun state, at most once per second.
fn log_debug_line() {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    let ms = T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1;
    let last = LAST_DEBUG_LOG.load(Relaxed);
    if last != 0 && ms < last + 1000 {
        return;
    }
    LAST_DEBUG_LOG.store(ms, Relaxed);
    match last_sun() {
        Some((u, v, c)) => {
            let (sky, md) = last_depth_info();
            log!("shafts: sun ({u:.3}, {v:.3}) confidence {c:.2}, sky fraction {sky:.3}, max depth {}", fmt_depth(md));
        }
        None => log!("shafts: sun not estimated yet"),
    }
}

/// The light-adjust substitute is about to draw on the game's context: computes the shaft light (once per
/// frame) and binds it as `t9` / `b8`. Never unwinds; every failure is logged once and leaves the frame without shafts.
pub fn on_light_adjust(ctx: Obj, dev: Obj) {
    let _ = std::panic::catch_unwind(|| unsafe { on_light_adjust_inner(ctx, dev) });
}

unsafe fn on_light_adjust_inner(ctx: Obj, dev: Obj) {
    static SKIP_COMPILING: AtomicBool = AtomicBool::new(false);
    static NO_DEPTH: AtomicBool = AtomicBool::new(false);
    static BAD_SCENE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    let _ig = IgnoreScope::new();
    let s = strength();
    // Off: nothing runs; if t9/b8 were bound before, take them away once.
    if s <= 0.0 || FAILED.load(Relaxed) {
        if BOUND.swap(false, Relaxed) {
            unsafe { unbind(ctx) };
        }
        return;
    }
    CUR.fetch_or(SEEN_DRAW, Relaxed);
    let skip = |why: &'static AtomicBool, flag: u8, msg: String| {
        log_once(why, || msg);
        CUR.fetch_or(flag, Relaxed);
        if BOUND.swap(false, Relaxed) {
            unsafe { unbind(ctx) };
        }
    };
    let blobs = match BLOBS.get() {
        None => {
            skip(&SKIP_COMPILING, 0, "shafts: skipped, the shaders are still compiling".to_string());
            return;
        }
        Some(Err(_)) => {
            FAILED.store(true, Relaxed);
            return;
        }
        Some(Ok(b)) => b,
    };
    let Ok(mut guard) = SHAFTS.try_lock() else { return };
    if guard.as_ref().is_some_and(|x| x.device != dev as usize) {
        *guard = None;
    }
    if guard.is_none() {
        match unsafe { Shafts::new(dev, blobs) } {
            Some(x) => *guard = Some(x),
            None => {
                FAILED.store(true, Relaxed);
                log!("shafts: could not be created; the feature stays off");
                return;
            }
        }
    }
    let sh = guard.as_mut().expect("just created");
    if !DONE.load(Relaxed) {
        let Some(depth) = ao::depth_resource() else {
            skip(&NO_DEPTH, SEEN_NO_DEPTH, "shafts: skipped, the linear depth of this frame is not available".to_string());
            return;
        };
        // The scene colour: the pixel shader SRV in slot 0 of the light-adjust draw.
        let mut scene_srv: Obj = std::ptr::null_mut();
        unsafe {
            let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            get(ctx, 0, 1, &mut scene_srv);
        }
        if scene_srv.is_null() {
            skip(&BAD_SCENE, SEEN_BAD_SCENE, "shafts: skipped, no scene colour is bound in slot 0".to_string());
            return;
        }
        let Some((res, d)) = (unsafe { ao::texture_of(scene_srv) }) else {
            unsafe { release(scene_srv) };
            skip(&BAD_SCENE, SEEN_BAD_SCENE, "shafts: skipped, the scene colour in slot 0 is not a 2D texture".to_string());
            return;
        };
        unsafe { release(res) };
        if d.samples != 1 || d.array != 1 || d.width < 16 || d.height < 16 {
            unsafe { release(scene_srv) };
            skip(&BAD_SCENE, SEEN_BAD_SCENE, format!("shafts: skipped, the scene colour is {}x{} format {} samples {}", d.width, d.height, d.format, d.samples));
            return;
        }
        let p = Params { width: d.width, height: d.height, length: length(), debug: matches!(crate::resolve::debug_level(), 1 | 4) };
        let ok = unsafe { sh.run(dev, ctx, scene_srv, depth, &p) };
        unsafe { release(scene_srv) };
        if !ok {
            skip(&NO_RUN, 0, "shafts: the targets, the depth view or the constants could not be set up; frame left without shafts".to_string());
            return;
        }
        DONE.store(true, Relaxed);
        CUR.fetch_or(SEEN_APPLIED, Relaxed);
        if crate::resolve::debug_level() == 4 {
            log_debug_line();
        }
        log_once(&FIRST, || format!("shafts: first frame applied (strength {}, length {}, scene {}x{})", s, length(), d.width, d.height));
    }
    if unsafe { sh.bind(ctx, s) } {
        BOUND.store(true, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(clamp_strength(9.0), 2.0);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), 0.6);
        assert_eq!(clamp_length(0.0), 0.3);
        assert_eq!(clamp_length(5.0), 1.0);
        assert_eq!(clamp_length(f32::NAN), 0.7);
    }

    #[test]
    fn quarter_size_rounds_up_and_never_zero() {
        assert_eq!(quarter(3440, 1440), (860, 360));
        assert_eq!(quarter(1921, 1081), (481, 271));
        assert_eq!(quarter(0, 0), (1, 1));
        assert_eq!(quarter(1, 3), (1, 1));
    }

    #[test]
    fn constants_follow_the_params() {
        let c = cb_values(&Params { width: 3440, height: 1440, length: 0.5, debug: true });
        assert_eq!(c[0], [860.0, 360.0, 1.0 / 860.0, 1.0 / 360.0]);
        assert_eq!((c[1][0], c[1][1], c[1][2]), (0.5, DECAY, 1.0));
        assert_eq!(c[2][0], 860.0 * 360.0);
        assert_eq!(c[3], [FAR_RATIO, MAX_INVALID_FOR_FAR, 0.0, 0.0]);
        let c = cb_values(&Params { width: 0, height: 0, length: 9.0, debug: false });
        assert_eq!((c[1][0], c[1][2]), (1.0, 0.0));
        assert!(c.iter().flatten().all(|v| v.is_finite()));
        assert_eq!(hp_values(0.8), [0.8, 0.0, 0.0, 0.0]);
        assert_eq!(hp_values(7.0)[0], 2.0);
        assert_eq!(hp_values(-1.0)[0], 0.0);
    }

    #[test]
    fn strength_zero_releases_the_depth_consumer_and_a_positive_one_registers_it() {
        let _g = crate::testlock::lock();
        // Only this test touches STRENGTH / LENGTH and the SHAFTS bit among the host tests.
        let (ks, kl) = (strength(), length());
        set_strength(0.0);
        assert!(ao::depth_consumers() & ao::DEPTH_FOR_SHAFTS == 0, "consumer must be released at strength 0");
        assert_eq!(strength(), 0.0);
        assert_eq!(set_strength(0.9), 0.9);
        assert!(ao::depth_consumers() & ao::DEPTH_FOR_SHAFTS != 0);
        assert_eq!(set_length(5.0), 1.0);
        assert_eq!(length(), 1.0);
        set_strength(0.0);
        assert!(status_text().starts_with("off"));
        set_strength(ks);
        set_length(kl);
        if ks <= 0.0 {
            ao::set_depth_consumer(ao::DEPTH_FOR_SHAFTS, false);
        }
    }
}

#[cfg(test)]
#[path = "shafts_tests.rs"]
mod gpu_tests;
