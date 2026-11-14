//! Screen-space contact shadows (`docs/contact-shadows.md`) and the shared `b10` of the light-adjust substitute.
//!
//! The engine's shadow maps are not available to the mod, so a short ray march in the engine's linear depth towards
//! the light finds what sits right next to a surface on the sun side (a rock on the ground, the edge of a roof, a
//! character's feet). It runs at the light-adjust draw (`datapath::bind_current` calls `on_light_adjust`), once per
//! frame, in this module's own half-size targets (the AO / shafts / fog pattern: nothing is drawn into the engine's
//! targets, no CPU read-back):
//!
//! 1. `ps_contact`: raw visibility, 20 steps over about 0.35 m (clamped to 1.2..5 % of the pixel depth) with the
//!    step start jittered by interleaved gradient noise (a fixed screen-space pattern, stable from frame to frame),
//! 2. `ps_blur`: 4x4 depth-aware blur, which removes the noise pattern without crossing depth edges.
//!
//! The result is bound as `t22` of the light-adjust substitute, which upsamples it depth-aware and multiplies the
//! diffuse light softly (never more than `contact_strength`), with the same release as the AO (fog, water, lights).
//! `b10` carries four numbers for both this and the aerial perspective: x contact strength, y aerial strength,
//! z aerial half distance (m), w metres per 1.0 of the engine's normalised linear depth (`depth_unit`, 1.0 = far plane =
//! sky; see `aerial::DEFAULT_UNIT`). Unbound or all zero = both off, bit-identical.
//!
//! The light direction comes from the sun shafts' estimate when it is found (`shafts::last_sun`), otherwise a fixed
//! direction from above (`contact_sun = fixed` forces it), at a reduced weight. Shaders are compiled once on a
//! background thread (never `D3DCompile` on the render thread).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Viewport, release, vfn, vt};
use crate::log;
use crate::shafts::{NO_TARGET, Target, make_target};
use crate::{aerial, ao, shaders, shafts};

pub const HLSL: &str = include_str!("../shaders/contact.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_CONTACT: &str = "ps_contact";
pub const PS_BLUR: &str = "ps_blur";

/// The darkening can never go beyond 80 % (the visibility rule: nothing may be hidden in a contact shadow).
pub const STRENGTH_RANGE: (f32, f32) = (0.0, 0.8);
pub const LENGTH_RANGE: (f32, f32) = (0.25, 2.0);
pub const DEFAULT_STRENGTH: f32 = 0.5;
pub const DEFAULT_LENGTH: f32 = 1.0;

/// Ray length in metres at `contact_length` 1.
pub const WORLD_LENGTH_M: f32 = 0.35;
/// The on-screen limits of the ray, as fractions of the pixel depth (so a wrong depth unit stays harmless).
pub const LEN_MIN_FRAC: f32 = 0.012;
pub const LEN_MAX_FRAC: f32 = 0.05;
/// An occluder counts when its surface is at most this many ray lengths nearer than the ray point (thin things only).
pub const THICKNESS: f32 = 2.5;
pub const STEPS: u32 = 20;
/// Depth bias as a fraction of the pixel depth (plus a slope-scaled part in the shader).
pub const BIAS_FRAC: f32 = 0.004;
/// tan(vertical half fov), the same assumption as the AO.
pub const TAN_HALF_FOV: f32 = ao::TAN_HALF_FOV;
/// Weight of the direction when it is a guess (the fixed one, or no sun found).
pub const FALLBACK_FADE: f32 = 0.6;
/// Share of `FALLBACK_FADE` kept when the shafts see no sky at all. It used to be 0 ("indoors: no sun"), but the most
/// common outdoor view, the camera looking down at the ground under trees, has no sky either, and real captures
/// showed `weight 0.00` there: the strength slider changed nothing. Indoors a soft contact shadow from above is harmless.
pub const NO_SKY_KEEP: f32 = 0.5;
/// A sun estimate counts as found from this confidence (the same threshold as the shafts' status line).
pub const SUN_FOUND: f32 = 0.05;
/// Confidence at which the found sun has its full weight.
pub const SUN_FULL: f32 = 0.35;
/// Fixed direction TO the light in view space (+z away from the camera): up, a little to the right and behind the camera.
pub const FIXED_DIR: [f32; 3] = [0.30, 0.85, -0.43];

/// Slots of the light-adjust substitute.
pub const SRV_SLOT: u32 = 22;
pub const CB_SLOT: u32 = 10;

const SLOT_FIRST: u32 = 10;
const SLOT_COUNT: u32 = 2; // t10, t11
const FMT_R16_FLOAT: u32 = 54;
const FMT_R32_FLOAT: u32 = 41;

// ---- switches ------------------------------------------------------------------------------------

static STRENGTH: AtomicU32 = AtomicU32::new(0x3f00_0000); // 0.5f32
static LENGTH: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static FIXED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
/// t22 / b10 are currently bound on the game's context (so they can be unbound when both features stop).
static BOUND: AtomicBool = AtomicBool::new(false);
static CUR: AtomicU8 = AtomicU8::new(0);
static LAST: AtomicU8 = AtomicU8::new(0);
const SEEN_DRAW: u8 = 1;
const SEEN_APPLIED: u8 = 2;
const SEEN_NO_DEPTH: u8 = 4;
const SEEN_BAD_SCENE: u8 = 8;
/// Where the last direction came from: 0 none, 1 the sun estimate, 2 the fixed direction (a guess).
static SRC: AtomicU8 = AtomicU8::new(0);
static FADE: AtomicU32 = AtomicU32::new(0);
/// The aerial inputs (t3 / t4 / b10) were bound at the last light-adjust draw.
static AERIAL_BOUND: AtomicBool = AtomicBool::new(false);

pub fn clamp_strength(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_STRENGTH } else { v.clamp(STRENGTH_RANGE.0, STRENGTH_RANGE.1) }
}

pub fn clamp_length(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_LENGTH } else { v.clamp(LENGTH_RANGE.0, LENGTH_RANGE.1) }
}

/// Sets the strength (live). 0 turns the feature off completely: no passes, and the depth consumer is released.
pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    ao::set_depth_consumer(ao::DEPTH_FOR_CONTACT, v > 0.0);
    v
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Contact, f32::from_bits(STRENGTH.load(Relaxed)))
}

pub fn set_length(v: f32) -> f32 {
    let v = clamp_length(v);
    LENGTH.store(v.to_bits(), Relaxed);
    v
}

pub fn length() -> f32 {
    f32::from_bits(LENGTH.load(Relaxed))
}

/// `contact_sun` text of the config: `fixed` forces the fixed direction, anything else is `auto`.
pub fn normalize_mode(v: &str) -> &'static str {
    if v.trim().eq_ignore_ascii_case("fixed") { "fixed" } else { "auto" }
}

pub fn set_mode(v: &str) -> &'static str {
    let m = normalize_mode(v);
    FIXED.store(m == "fixed", Relaxed);
    m
}

pub fn mode() -> &'static str {
    if FIXED.load(Relaxed) { "fixed" } else { "auto" }
}

/// Applies the config values at start and starts the background compile.
pub fn init(strength: f32, length: f32, sun: &str) {
    set_mode(sun);
    set_length(length);
    set_strength(strength);
    log!("contact: configured (strength {}, length {}, sun {})", self::strength(), self::length(), mode());
    let _ = std::thread::Builder::new().name("gw2g-contact-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("contact: shaders compiled"),
            Err(e) => log!("contact: shader compile failed, contact shadows stay off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

/// A new frame starts: remembers what the last one did (overlay), allows the passes to run again.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
    LAST.store(CUR.swap(0, Relaxed), Relaxed);
}

// ---- the light direction -------------------------------------------------------------------------------

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l > 1e-6 { [v[0] / l, v[1] / l, v[2] / l] } else { normalize3(FIXED_DIR) }
}

/// View-space direction towards the sun seen at screen position (`u`, `v`) (0..1, v down): the ray through that pixel.
pub fn sun_direction(u: f32, v: f32, aspect: f32) -> [f32; 3] {
    let (u, v) = (if u.is_finite() { u.clamp(0.0, 1.0) } else { 0.5 }, if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.5 });
    normalize3([(2.0 * u - 1.0) * TAN_HALF_FOV * aspect.max(0.1), (1.0 - 2.0 * v) * TAN_HALF_FOV, 1.0])
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Where the direction came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Sun,
    Fixed,
}

/// The light direction (view space) and its weight 0..1 for this frame.
/// `sun` = the shafts' last estimate (u, v, confidence) when the shafts run; `sky` = their sky fraction (None = the
/// shafts are off, nothing is known about the sky).
///
/// - `fixed`: the fixed direction at `FALLBACK_FADE`.
/// - `auto` with a sun found: its direction, weight rising with the confidence (0.05 -> 0, 0.35 -> 1).
/// - `auto` without: the fixed direction at `FALLBACK_FADE`, scaled down to `NO_SKY_KEEP` of it when the shafts say
///   there is hardly any sky in the picture (indoors, caves, or looking down at the ground); when nothing is known it
///   keeps `FALLBACK_FADE`.
pub fn plan(fixed: bool, sun: Option<(f32, f32, f32)>, sky: Option<f32>, aspect: f32) -> ([f32; 3], f32, Source) {
    if !fixed {
        if let Some((u, v, c)) = sun {
            if c >= SUN_FOUND && c.is_finite() {
                return (sun_direction(u, v, aspect), smoothstep(SUN_FOUND, SUN_FULL, c), Source::Sun);
            }
        }
    }
    let mut fade = FALLBACK_FADE;
    if !fixed {
        if let Some(s) = sky {
            if s.is_finite() {
                fade *= NO_SKY_KEEP + (1.0 - NO_SKY_KEEP) * smoothstep(0.0, 0.08, s);
            }
        }
    }
    (normalize3(FIXED_DIR), fade, Source::Fixed)
}

// ---- constants --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Full scene size in pixels (the passes run at half of it).
    pub width: u32,
    pub height: u32,
    /// Direction towards the light, view space, normalized.
    pub dir: [f32; 3],
    /// Weight of the direction (0..1).
    pub fade: f32,
    /// The `contact_length` slider.
    pub length: f32,
    /// Metres per 1.0 of the normalised depth (`depth_unit`; old inch-based values fall back to the default).
    pub unit: f32,
}

/// Half-size target dimensions of a scene of `w` x `h`.
pub fn half(w: u32, h: u32) -> (u32, u32) {
    (w.max(1).div_ceil(2), h.max(1).div_ceil(2))
}

/// The four constant-buffer vectors of `contact.hlsl` (see its header).
pub fn cb_values(p: &Params) -> [[f32; 4]; 4] {
    let (w, h) = (p.width.max(1) as f32, p.height.max(1) as f32);
    let unit = aerial::clamp_unit(p.unit);
    let d = normalize3(p.dir);
    [
        [d[0], d[1], d[2], if p.fade.is_finite() { p.fade.clamp(0.0, 1.0) } else { 0.0 }],
        [1.0 / w, 1.0 / h, w, h],
        [WORLD_LENGTH_M * clamp_length(p.length) / unit, LEN_MIN_FRAC, LEN_MAX_FRAC, THICKNESS],
        [TAN_HALF_FOV, w / h, STEPS as f32, BIAS_FRAC],
    ]
}

/// The `b10` of the light-adjust substitute: x contact strength, y aerial strength, z aerial half distance (m), w metres per 1.0 of the normalised depth.
pub fn b10_values(contact: f32, aerial_strength: f32, aerial_distance: f32, unit: f32) -> [f32; 4] {
    let f = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
    [
        f(contact, STRENGTH_RANGE.0, STRENGTH_RANGE.1, 0.0),
        f(aerial_strength, aerial::STRENGTH_RANGE.0, aerial::STRENGTH_RANGE.1, 0.0),
        f(aerial_distance, aerial::DISTANCE_RANGE.0, aerial::DISTANCE_RANGE.1, aerial::DEFAULT_DISTANCE),
        aerial::clamp_unit(unit),
    ]
}

// ---- shaders (compiled once, in the background) -----------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub contact: Vec<u8>,
    pub blur: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, contact: c(PS_CONTACT, "ps_5_0")?, blur: c(PS_BLUR, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

// ---- device objects ---------------------------------------------------------------------------------

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

/// Device objects of the contact shadows, valid for one device only.
pub struct Contact {
    device: usize,
    vs: Obj,
    ps_contact: Obj,
    ps_blur: Obj,
    cb: Obj,
    cb10: Obj,
    /// Scene size the targets were made for.
    size: (u32, u32),
    raw: Target,
    out: Target,
    /// SRV of the depth resource it was made for.
    depth_srv: Obj,
    depth_key: usize,
}

// The pointers are only used under the mutex, from the render thread.
unsafe impl Send for Contact {}

impl Contact {
    /// Creates the constant buffers and, when the shaders are compiled (`b`), the shaders, on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: Option<&Blobs>) -> Option<Contact> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut c = Contact { device: device as usize, vs: nul, ps_contact: nul, ps_blur: nul, cb: nul, cb10: nul, size: (0, 0), raw: NO_TARGET, out: NO_TARGET, depth_srv: nul, depth_key: 0 };
        unsafe {
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &[16, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut c.cb10) < 0 || c.cb10.is_null() {
                log!("contact: could not create the b10 constant buffer");
                return None;
            }
            if let Some(b) = b {
                let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
                let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
                let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut c.vs) >= 0
                    && create_ps(device, b.contact.as_ptr(), b.contact.len(), nul, &mut c.ps_contact) >= 0
                    && create_ps(device, b.blur.as_ptr(), b.blur.len(), nul, &mut c.ps_blur) >= 0;
                if !ok || create_buffer(device, &[64, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut c.cb) < 0 || c.cb.is_null() {
                    log!("contact: could not create the shaders");
                    return None; // Drop releases what exists
                }
            }
        }
        Some(c)
    }

    /// The pass shaders exist (the compile had finished when this was created).
    pub fn has_shaders(&self) -> bool {
        !self.ps_contact.is_null()
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            self.raw.free();
            self.out.free();
        }
        self.size = (0, 0);
    }

    /// (Re)creates the half-size targets when the scene size changed.
    unsafe fn ensure_targets(&mut self, dev: Obj, size: (u32, u32)) -> bool {
        if self.size == size && !self.raw.tex.is_null() {
            return true;
        }
        unsafe {
            self.free_targets();
            let (hw, hh) = half(size.0, size.1);
            let (Some(raw), Some(out)) = (make_target(dev, hw, hh, FMT_R16_FLOAT, false), make_target(dev, hw, hh, FMT_R16_FLOAT, false)) else {
                return false;
            };
            self.raw = raw;
            self.out = out;
        }
        self.size = size;
        log!("contact: targets {}x{} (scene {}x{})", half(size.0, size.1).0, half(size.0, size.1).1, size.0, size.1);
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

    /// SRV of the finished visibility (valid after `run`, until the targets are recreated).
    #[allow(dead_code)] // used by the tests; production binds it through `bind`
    pub fn out_srv(&self) -> Obj {
        self.out.srv
    }

    /// The finished visibility texture and the raw (noisy) one, for the tests.
    #[allow(dead_code)]
    pub fn textures(&self) -> (Obj, Obj) {
        (self.out.tex, self.raw.tex)
    }

    /// Runs the two draws from the linear `depth`. All pipeline state touched is restored. False when something
    /// failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `depth` an R32_FLOAT texture of
    /// `p.width` x `p.height` with a shader-resource bind.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, depth: Obj, p: &Params) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.has_shaders() || !self.ensure_targets(dev, (p.width, p.height)) || !self.ensure_depth_srv(dev, depth) {
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
            let (hw, hh) = half(p.width, p.height);
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
            let nul: Obj = std::ptr::null_mut();
            let clear = || set_srv(ctx, SLOT_FIRST, SLOT_COUNT, [nul; SLOT_COUNT as usize].as_ptr());

            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, factor.as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);
            set_vp(ctx, 1, &vp);

            // 1: depth -> raw visibility.
            clear();
            set_rt(ctx, 1, &self.raw.rtv, nul);
            set_ps(ctx, self.ps_contact, std::ptr::null(), 0);
            set_srv(ctx, 10, 1, &self.depth_srv);
            draw(ctx, 3, 0);
            // 2: raw visibility + depth -> blurred visibility.
            clear();
            set_rt(ctx, 1, &self.out.rtv, nul);
            set_ps(ctx, self.ps_blur, std::ptr::null(), 0);
            set_srv(ctx, 10, 1, &self.depth_srv);
            set_srv(ctx, 11, 1, &self.raw.srv);
            draw(ctx, 3, 0);
            // Leave nothing of ours bound (the guard restores t0, t1 and b0).
            clear();
        }
        true
    }

    /// Writes `b10` and binds `t22` (the visibility, or nothing) and `b10` on `ctx`.
    ///
    /// # Safety
    /// `ctx` must be a live immediate context of this device; `vis` a live SRV of it.
    pub unsafe fn bind(&self, ctx: Obj, vis: Option<Obj>, b10: [f32; 4]) -> bool {
        unsafe {
            let mut mapped: [usize; 3] = [0; 3];
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            if map(ctx, self.cb10, 0, 4, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [f32; 4]) = b10;
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            unmap(ctx, self.cb10, 0);
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            let v = vis.unwrap_or(std::ptr::null_mut());
            set_srv(ctx, SRV_SLOT, 1, &v);
            set_cb(ctx, CB_SLOT, 1, &self.cb10);
        }
        true
    }
}

/// Unbinds `t22` and `b10` (they then read zero: no contact shadows, no aerial perspective).
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

impl Drop for Contact {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            self.free_targets();
            for o in [self.depth_srv, self.cb10, self.cb, self.ps_blur, self.ps_contact, self.vs] {
                release(o);
            }
        }
    }
}

// ---- status -------------------------------------------------------------------------------------------

/// Whether the fog estimate must run for these features (the light-adjust shader needs t3 / t4 for the release).
pub fn needs_fog() -> bool {
    strength() > 0.0 || aerial::strength() > 0.0
}

/// `aerial::status_text` helper.
pub fn aerial_state() -> &'static str {
    if aerial::strength() <= 0.0 {
        "off"
    } else if AERIAL_BOUND.load(Relaxed) {
        "on (inputs bound; needs the fog colour estimate and the linear depth)"
    } else {
        "waiting for a scene frame"
    }
}

/// Overlay line / dev capture `status_contact=`.
pub fn status_text() -> String {
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    if strength() <= 0.0 {
        return "off (strength 0)".to_string();
    }
    let l = LAST.load(Relaxed);
    if l & SEEN_APPLIED != 0 {
        let src = match SRC.load(Relaxed) {
            1 => "sun estimate",
            2 => "fixed direction (a guess)",
            _ => "no direction",
        };
        return format!("applied, light from the {src}, weight {:.2}, mode {}, strength {:.2}, length {:.2} ({:.2} m at depth unit {} m)", f32::from_bits(FADE.load(Relaxed)), mode(), strength(), length(), WORLD_LENGTH_M * length(), aerial::unit());
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

// ---- per-frame glue (called from datapath::bind_current) ------------------------------------------------

static CONTACT: Mutex<Option<Contact>> = Mutex::new(None);

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// The light-adjust substitute is about to draw on the game's context: computes the contact visibility (once per
/// frame) and binds `t22` / `b10` (also for the aerial perspective, which needs only `b10`). Never unwinds; every
/// failure is logged once and leaves the frame without the effects.
pub fn on_light_adjust(ctx: Obj, dev: Obj) {
    let _ = std::panic::catch_unwind(|| unsafe { on_light_adjust_inner(ctx, dev) });
}

unsafe fn on_light_adjust_inner(ctx: Obj, dev: Obj) {
    static NO_DEPTH: AtomicBool = AtomicBool::new(false);
    static BAD_SCENE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    let _ig = IgnoreScope::new();
    let on = crate::gate::user_enabled() && ao::enabled() && !FAILED.load(Relaxed);
    let cs = if on { strength() } else { 0.0 };
    let aer = if on { aerial::strength() } else { 0.0 };
    let off = |ctx: Obj| {
        AERIAL_BOUND.store(false, Relaxed);
        if BOUND.swap(false, Relaxed) {
            unsafe { unbind(ctx) };
        }
    };
    if cs <= 0.0 && aer <= 0.0 {
        off(ctx);
        return;
    }
    if cs > 0.0 {
        CUR.fetch_or(SEEN_DRAW, Relaxed);
    }
    let blobs = match BLOBS.get() {
        Some(Ok(b)) => Some(b),
        Some(Err(_)) => {
            if cs > 0.0 {
                FAILED.store(true, Relaxed);
            }
            None
        }
        None => None,
    };
    let Ok(mut guard) = CONTACT.try_lock() else { return };
    // A new device, or the shaders became available after the object was made without them.
    if guard.as_ref().is_some_and(|x| x.device != dev as usize || (blobs.is_some() && !x.has_shaders())) {
        *guard = None;
    }
    if guard.is_none() {
        match unsafe { Contact::new(dev, blobs) } {
            Some(x) => *guard = Some(x),
            None => {
                FAILED.store(true, Relaxed);
                log!("contact: could not be created; contact shadows and the aerial perspective stay off");
                off(ctx);
                return;
            }
        }
    }
    let c = guard.as_mut().expect("just created");
    let mut vis: Option<Obj> = None;
    if cs > 0.0 && c.has_shaders() {
        if !DONE.load(Relaxed) {
            'run: {
                let Some(depth) = ao::depth_resource() else {
                    log_once(&NO_DEPTH, || "contact: skipped, the linear depth of this frame is not available".to_string());
                    CUR.fetch_or(SEEN_NO_DEPTH, Relaxed);
                    break 'run;
                };
                // The scene size: the pixel shader SRV in slot 0 of the light-adjust draw.
                let mut scene_srv: Obj = std::ptr::null_mut();
                unsafe {
                    let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
                    get(ctx, 0, 1, &mut scene_srv);
                }
                if scene_srv.is_null() {
                    log_once(&BAD_SCENE, || "contact: skipped, no scene colour is bound in slot 0".to_string());
                    CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
                    break 'run;
                }
                let tex = unsafe { ao::texture_of(scene_srv) };
                unsafe { release(scene_srv) };
                let Some((res, d)) = tex else {
                    log_once(&BAD_SCENE, || "contact: skipped, the scene colour in slot 0 is not a 2D texture".to_string());
                    CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
                    break 'run;
                };
                unsafe { release(res) };
                if d.samples != 1 || d.array != 1 || d.width < 16 || d.height < 16 {
                    log_once(&BAD_SCENE, || format!("contact: skipped, the scene colour is {}x{} format {} samples {}", d.width, d.height, d.format, d.samples));
                    CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
                    break 'run;
                }
                let aspect = d.width as f32 / d.height as f32;
                let shafts_on = shafts::strength() > 0.0;
                let sun = if shafts_on { shafts::last_sun() } else { None };
                let sky = if shafts_on && sun.is_some() { Some(shafts::last_depth_info().0) } else { None };
                let (dir, fade, src) = plan(FIXED.load(Relaxed), sun, sky, aspect);
                let p = Params { width: d.width, height: d.height, dir, fade, length: length(), unit: aerial::unit() };
                if !unsafe { c.run(dev, ctx, depth, &p) } {
                    log_once(&NO_RUN, || "contact: the targets, the depth view or the constants could not be set up; frame left without contact shadows".to_string());
                    break 'run;
                }
                DONE.store(true, Relaxed);
                CUR.fetch_or(SEEN_APPLIED, Relaxed);
                SRC.store(if src == Source::Sun { 1 } else { 2 }, Relaxed);
                FADE.store(fade.to_bits(), Relaxed);
                log_once(&FIRST, || format!("contact: first frame applied (strength {}, length {}, mode {}, scene {}x{})", cs, length(), mode(), d.width, d.height));
            }
        }
        if DONE.load(Relaxed) {
            vis = Some(c.out_srv());
        }
    }
    let b10 = b10_values(if vis.is_some() { cs } else { 0.0 }, aer, aerial::distance(), aerial::unit());
    if unsafe { c.bind(ctx, vis, b10) } {
        BOUND.store(true, Relaxed);
        AERIAL_BOUND.store(aer > 0.0, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(clamp_strength(9.0), 0.8);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), 0.5);
        assert_eq!(clamp_length(0.0), 0.25);
        assert_eq!(clamp_length(9.0), 2.0);
        assert_eq!(clamp_length(f32::NAN), 1.0);
        assert_eq!(f32::from_bits(0x3f00_0000), DEFAULT_STRENGTH);
        assert_eq!(f32::from_bits(0x3f80_0000), DEFAULT_LENGTH);
        assert_eq!(normalize_mode("FIXED"), "fixed");
        assert_eq!(normalize_mode(" Fixed "), "fixed");
        assert_eq!(normalize_mode("auto"), "auto");
        assert_eq!(normalize_mode("whatever"), "auto");
        assert_eq!(normalize_mode(""), "auto");
    }

    #[test]
    fn strength_zero_releases_the_depth_consumer_and_a_positive_one_registers_it() {
        let (ks, kl, km) = (strength(), length(), mode());
        set_strength(0.0);
        assert_eq!(ao::depth_consumers() & ao::DEPTH_FOR_CONTACT, 0, "consumer must be released at strength 0");
        assert!(!needs_fog() || aerial::strength() > 0.0);
        assert_eq!(set_strength(0.7), 0.7);
        assert_ne!(ao::depth_consumers() & ao::DEPTH_FOR_CONTACT, 0);
        assert!(needs_fog());
        assert_eq!(set_length(5.0), 2.0);
        assert_eq!(set_mode("fixed"), "fixed");
        assert_eq!(mode(), "fixed");
        set_strength(0.0);
        assert!(status_text().starts_with("off"));
        set_strength(ks);
        set_length(kl);
        set_mode(km);
        if ks <= 0.0 {
            ao::set_depth_consumer(ao::DEPTH_FOR_CONTACT, false);
        }
    }

    #[test]
    fn sun_direction_points_through_the_pixel() {
        let c = sun_direction(0.5, 0.5, 2.0);
        assert!((c[0].abs() + c[1].abs()) < 1e-6 && (c[2] - 1.0).abs() < 1e-6, "{c:?}");
        let up = sun_direction(0.5, 0.0, 2.0);
        assert!(up[1] > 0.4 && up[2] > 0.8, "{up:?}");
        let right = sun_direction(1.0, 0.5, 2.0);
        assert!(right[0] > 0.4, "{right:?}");
        for d in [sun_direction(f32::NAN, 9.0, 0.0), sun_direction(-3.0, -3.0, 1.0)] {
            assert!(d.iter().all(|x| x.is_finite()));
            assert!((d[0] * d[0] + d[1] * d[1] + d[2] * d[2] - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn plan_uses_the_sun_when_found_and_fades_with_the_confidence() {
        let (d, f, s) = plan(false, Some((0.8, 0.2, 0.9)), Some(0.3), 2.0);
        assert_eq!((s, f), (Source::Sun, 1.0));
        assert_eq!(d, sun_direction(0.8, 0.2, 2.0));
        let (_, f_low, s) = plan(false, Some((0.8, 0.2, 0.15)), Some(0.3), 2.0);
        assert_eq!(s, Source::Sun);
        assert!(f_low > 0.0 && f_low < 1.0, "{f_low}");
        // Below the found threshold: the fixed direction at the guess weight.
        let (d, f, s) = plan(false, Some((0.8, 0.2, 0.01)), Some(0.3), 2.0);
        assert_eq!((s, f), (Source::Fixed, FALLBACK_FADE));
        assert!((d[0] * d[0] + d[1] * d[1] + d[2] * d[2] - 1.0).abs() < 1e-5);
        // `fixed` ignores a found sun.
        let (_, f, s) = plan(true, Some((0.8, 0.2, 0.9)), Some(0.3), 2.0);
        assert_eq!((s, f), (Source::Fixed, FALLBACK_FADE));
        // No sky in the picture (indoors, or looking down at the ground): the guess is halved (it used to fade out to 0,
        // which left the strength slider without any effect in such views); nothing known: the guess keeps its weight.
        assert_eq!(plan(false, Some((0.5, 0.5, 0.0)), Some(0.0), 2.0).1, FALLBACK_FADE * NO_SKY_KEEP);
        assert_eq!(plan(false, Some((0.5, 0.5, 0.0)), Some(0.5), 2.0).1, FALLBACK_FADE);
        assert_eq!(plan(false, None, None, 2.0).1, FALLBACK_FADE);
        assert_eq!(plan(false, None, Some(f32::NAN), 2.0).1, FALLBACK_FADE);
    }

    #[test]
    fn constants_follow_the_params() {
        // (The unit is metres per 1.0 of the normalised depth now; the old test used 0.0254, which is now "unset".)
        let p = Params { width: 3440, height: 1440, dir: [0.0, 3.0, 4.0], fade: 0.8, length: 2.0, unit: 1000.0 };
        let c = cb_values(&p);
        assert!((c[0][1] - 0.6).abs() < 1e-6 && (c[0][2] - 0.8).abs() < 1e-6 && c[0][3] == 0.8);
        assert_eq!(c[1], [1.0 / 3440.0, 1.0 / 1440.0, 3440.0, 1440.0]);
        assert!((c[2][0] - 0.35 * 2.0 / 1000.0).abs() < 1e-9);
        // An old inch-based unit is replaced by the default: 0.7 m of ray, not 27.6 units (which hit the 5 % clamp).
        assert_eq!(cb_values(&Params { unit: 0.0254, ..p })[2][0], c[2][0]);
        // At 9 m (the charr of a real capture, depth 0.009) the 0.35 m ray lies inside the on-screen clamps.
        let z = 0.009f32;
        let len = 0.35 / aerial::DEFAULT_UNIT;
        assert!(len > LEN_MIN_FRAC * z && len < LEN_MAX_FRAC * z, "{len}");
        assert_eq!((c[2][1], c[2][2], c[2][3]), (LEN_MIN_FRAC, LEN_MAX_FRAC, THICKNESS));
        assert_eq!((c[3][0], c[3][2], c[3][3]), (TAN_HALF_FOV, STEPS as f32, BIAS_FRAC));
        let bad = cb_values(&Params { width: 0, height: 0, dir: [0.0; 3], fade: f32::NAN, length: f32::NAN, unit: f32::NAN });
        assert!(bad.iter().flatten().all(|v| v.is_finite()));
        assert_eq!(bad[0][3], 0.0);
        assert_eq!(half(3440, 1440), (1720, 720));
        assert_eq!(half(1921, 1081), (961, 541));
        assert_eq!(half(0, 0), (1, 1));
    }

    #[test]
    fn b10_is_clamped_and_ordered() {
        // (Unit = metres per 1.0 of the normalised depth; old inch-based values <= 2 mean the default.)
        assert_eq!(b10_values(0.5, 0.35, 400.0, 1000.0), [0.5, 0.35, 400.0, 1000.0]);
        assert_eq!(b10_values(0.5, 0.35, 400.0, 0.0254), [0.5, 0.35, 400.0, aerial::DEFAULT_UNIT]);
        assert_eq!(b10_values(9.0, 9.0, 1.0, 0.0), [0.8, 1.0, 50.0, aerial::DEFAULT_UNIT]);
        assert_eq!(b10_values(f32::NAN, f32::INFINITY, f32::NAN, f32::NAN), [0.0, 0.0, 400.0, aerial::DEFAULT_UNIT]);
        assert_eq!(b10_values(-1.0, -1.0, 1e9, 9.0), [0.0, 0.0, 5000.0, aerial::UNIT_RANGE.0]);
        assert_eq!(b10_values(0.0, 0.0, 400.0, 1e9), [0.0, 0.0, 400.0, aerial::UNIT_RANGE.1]);
    }
}

#[cfg(test)]
#[path = "contact_tests.rs"]
mod gpu_tests;
