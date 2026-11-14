//! Screen-space ambient occlusion (M2). See `docs/m2-ao.md` for the engine facts it relies on.
//!
//! Where: at the start of the effects pass (#9 of the reference frame, found structurally by
//! `classify::ao_stage`), i.e. after the opaque pass and the engine's lights, BEFORE the transparents,
//! the light adjustment, the bloom and the combine. The AO multiplies the scene colour in place with a
//! blend state (result = dest * src), so no scratch copy is needed, and what is drawn afterwards (effects,
//! particles) is not darkened. Depth: the engine's own full-size `R32_FLOAT` linear depth target, captured
//! when its pass starts (`AoStage::DepthLinear`) and sampled as a shader resource.
//!
//! Live settings are two atomics copied into a constant buffer on every draw: moving a slider never
//! compiles anything. The shaders are compiled once, on a background thread started by `init`; until that
//! has finished the AO is simply skipped (the render thread never calls D3DCompile).
//!
//! Plain decisions (`clamp_*`, `cb_values`, `wants`) are host-testable; `Ao` talks to D3D through raw
//! vtables like `resolve.rs` and is exercised on WARP by `ao_tests.rs`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, RESOURCE_TEXTURE2D, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::log;
use crate::passes::{FMT_B8G8R8A8_UNORM, FMT_R16G16B16A16_FLOAT, FMT_R32_FLOAT};
use crate::shaders;

pub const HLSL: &str = include_str!("../shaders/ao.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_AO: &str = "ps_ao";
pub const PS_BLUR: &str = "ps_blur_h";
pub const PS_APPLY: &str = "ps_apply";
pub const PS_BLUR_V: &str = "ps_blur_v";

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 2.0);
pub const RADIUS_RANGE: (f32, f32) = (0.25, 4.0);
pub const DEFAULT_STRENGTH: f32 = 1.0;
pub const DEFAULT_RADIUS: f32 = 1.0;

/// tan(vertical half fov) assumed for the view-space reconstruction (about 53 degrees). The engine's
/// projection is not read; only the shape of the result depends on this, not its validity.
pub const TAN_HALF_FOV: f32 = 0.5;
/// Largest darkening of the encoded colour: the multiply factor never goes below `1 - MAX_DARKEN` = 0.5,
/// whatever the strength (the visibility rule: AO cannot hide anything).
pub const MAX_DARKEN: f32 = 0.5;
/// Samples whose direction is within this of the surface plane do not occlude (flat surfaces stay flat).
pub const NORMAL_BIAS: f32 = 0.05;
/// World radius per slider unit as a fraction of the pixel's depth.
pub const RADIUS_BASE: f32 = 0.03;
pub const GAIN: f32 = 3.5;

const R16_FLOAT: u32 = 54;
const R32_TYPELESS: u32 = 39;

// ---- switches --------------------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static STRENGTH: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static RADIUS: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
/// What happened this frame / last frame (bit flags below).
static CUR: AtomicU8 = AtomicU8::new(0);
static LAST: AtomicU8 = AtomicU8::new(0);
const SEEN_DEPTH: u8 = 1;
const SEEN_APPLIED: u8 = 2;
const SEEN_EFFECTS: u8 = 4;

/// Clamps to 0..=2; NaN gives the default (1).
pub fn clamp_strength(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_STRENGTH } else { v.clamp(STRENGTH_RANGE.0, STRENGTH_RANGE.1) }
}

/// Clamps to 0.25..=4; NaN gives the default (1).
pub fn clamp_radius(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_RADIUS } else { v.clamp(RADIUS_RANGE.0, RADIUS_RANGE.1) }
}

pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    v
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Ao, f32::from_bits(STRENGTH.load(Relaxed)))
}

pub fn set_radius(v: f32) -> f32 {
    let v = clamp_radius(v);
    RADIUS.store(v.to_bits(), Relaxed);
    v
}

pub fn radius() -> f32 {
    f32::from_bits(RADIUS.load(Relaxed))
}

/// Turned on and off at the frame boundary together with the HDR resolve (the classifier's gate).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// On (gate open, user enabled), and no unrecoverable failure so far.
pub fn enabled() -> bool {
    ENABLED.load(Relaxed) && !FAILED.load(Relaxed)
}

/// The AO should look at passes: on, strength above 0 (0 disables the whole pass), not done this frame.
pub fn wants() -> bool {
    enabled() && strength() > 0.0 && !DONE.load(Relaxed)
}

// ---- the engine's linear depth, shared with the other screen-space features ---------------------------------

/// Bits for `set_depth_consumer`: other features that need the frame's linear depth even when the AO is off.
pub const DEPTH_FOR_SSR: u32 = 1;
pub const DEPTH_FOR_SHAFTS: u32 = 2;
pub const DEPTH_FOR_FUR: u32 = 4;
pub const DEPTH_FOR_EXPOSURE: u32 = 8;
pub const DEPTH_FOR_CONTACT: u32 = 16;
pub const DEPTH_FOR_AERIAL: u32 = 32;
pub const DEPTH_FOR_STATS: u32 = 64;
pub const DEPTH_FOR_RIM: u32 = 128;

static EXTRA_DEPTH: AtomicU32 = AtomicU32::new(0);

/// A feature that reads the linear depth (`depth_resource`) says whether it is on. While any is on (and the gate
/// is open) the pass tracking and the depth capture run even with `ao_strength` 0.
pub fn set_depth_consumer(bit: u32, on: bool) {
    if on {
        EXTRA_DEPTH.fetch_or(bit, Relaxed);
    } else {
        EXTRA_DEPTH.fetch_and(!bit, Relaxed);
    }
}

/// The depth consumer bits that are on (`DEPTH_FOR_*`); the tests of the consumers read it.
#[allow(dead_code)]
pub fn depth_consumers() -> u32 {
    EXTRA_DEPTH.load(Relaxed)
}

/// Every consumer bit, for the distinctness test.
#[allow(dead_code)]
pub const ALL_DEPTH_CONSUMERS: [u32; 8] = [DEPTH_FOR_SSR, DEPTH_FOR_SHAFTS, DEPTH_FOR_FUR, DEPTH_FOR_EXPOSURE, DEPTH_FOR_CONTACT, DEPTH_FOR_AERIAL, DEPTH_FOR_STATS, DEPTH_FOR_RIM];

/// The pass tracker should run: gate open, not done this frame, and either the AO or a depth consumer is on.
pub fn needs_stages() -> bool {
    enabled() && !DONE.load(Relaxed) && (strength() > 0.0 || EXTRA_DEPTH.load(Relaxed) != 0)
}

/// The linear depth resource of THIS frame (full-size R32_FLOAT, shader-readable), found structurally at the
/// start of the depth pass. No reference is taken: the pointer is valid until the next `frame_begin`. None before
/// the depth pass of the frame, when the gate is closed, or when no usable depth was found.
pub fn depth_resource() -> Option<Obj> {
    let s = DEPTH.try_lock().ok()?;
    (s.0 != 0).then(|| s.0 as Obj)
}

/// A new frame starts: remembers what the last one did (overlay), forgets its depth target.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
    LAST.store(CUR.swap(0, Relaxed), Relaxed);
    if let Ok(mut s) = DEPTH.try_lock() {
        unsafe { release(s.0 as Obj) };
        s.0 = 0;
    }
}

/// Overlay line.
pub fn status_text() -> String {
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    if strength() <= 0.0 {
        return "off (strength 0)".to_string();
    }
    if !ENABLED.load(Relaxed) {
        return "waiting (classifier gate closed or mod disabled)".to_string();
    }
    let l = LAST.load(Relaxed);
    if l & SEEN_APPLIED != 0 {
        "applied".to_string()
    } else if l & SEEN_EFFECTS != 0 && l & SEEN_DEPTH == 0 {
        "skipped: depth not found".to_string()
    } else if l & SEEN_EFFECTS != 0 {
        "skipped: could not run (see log)".to_string()
    } else if BLOBS.get().is_none() {
        "compiling shaders".to_string()
    } else {
        "waiting for a scene frame".to_string()
    }
}

// ---- constants --------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AoParams {
    pub strength: f32,
    pub radius: f32,
    pub width: u32,
    pub height: u32,
}

/// The three constant-buffer vectors of `ao.hlsl` (see its header).
pub fn cb_values(p: &AoParams) -> [[f32; 4]; 3] {
    let (w, h) = (p.width.max(1) as f32, p.height.max(1) as f32);
    [
        [if crate::resolve::debug_level() == 2 { -(clamp_strength(p.strength) + 10.0) } else if crate::resolve::debug_level() == 1 { -clamp_strength(p.strength) } else { clamp_strength(p.strength) }, clamp_radius(p.radius), TAN_HALF_FOV, w / h],
        [1.0 / w, 1.0 / h, w, h],
        [MAX_DARKEN, NORMAL_BIAS, RADIUS_BASE, GAIN],
    ]
}

// ---- shaders (compiled once, in the background) -----------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub ao: Vec<u8>,
    pub blur: Vec<u8>,
    pub apply: Vec<u8>,
    pub blur_v: Vec<u8>,
}

/// Compiles the four shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, ao: c(PS_AO, "ps_5_0")?, blur: c(PS_BLUR, "ps_5_0")?, apply: c(PS_APPLY, "ps_5_0")?, blur_v: c(PS_BLUR_V, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Sets the live values from the config and starts the background compile.
pub fn init(strength: f32, radius: f32) {
    set_strength(strength);
    set_radius(radius);
    log!("ao: configured (strength {}, radius {})", self::strength(), self::radius());
    let _ = std::thread::Builder::new().name("gw2g-ao-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("ao: shaders compiled"),
            Err(e) => log!("ao: shader compile failed, AO stays off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- the AO objects -------------------------------------------------------------------------------

/// Device objects of the AO, valid for one device only.
pub struct Ao {
    device: usize,
    vs: Obj,
    ps_ao: Obj,
    ps_blur: Obj,
    ps_apply: Obj,
    ps_blur_v: Obj,
    cb: Obj,
    multiply: Obj,
    /// Visibility targets (R16_FLOAT, scene size): texture, SRV, RTV. 0 and 1 are the ping-pong pair;
    /// 2 holds the finished, blurred visibility that the data path samples.
    tex: [Obj; 3],
    srv: [Obj; 3],
    rtv: [Obj; 3],
    size: (u32, u32),
    /// SRV of the depth resource it was made for.
    depth_srv: Obj,
    depth_key: usize,
}

// The pointers are only used under the AO mutex, from the render thread.
unsafe impl Send for Ao {}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

impl Ao {
    /// Creates the shaders, the constant buffer and the multiply blend state on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Ao> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut a = Ao { device: device as usize, vs: nul, ps_ao: nul, ps_blur: nul, ps_apply: nul, ps_blur_v: nul, cb: nul, multiply: nul, tex: [nul; 3], srv: [nul; 3], rtv: [nul; 3], size: (0, 0), depth_srv: nul, depth_key: 0 };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut a.vs) >= 0
                && create_ps(device, b.ao.as_ptr(), b.ao.len(), nul, &mut a.ps_ao) >= 0
                && create_ps(device, b.blur.as_ptr(), b.blur.len(), nul, &mut a.ps_blur) >= 0
                && create_ps(device, b.apply.as_ptr(), b.apply.len(), nul, &mut a.ps_apply) >= 0
                && create_ps(device, b.blur_v.as_ptr(), b.blur_v.len(), nul, &mut a.ps_blur_v) >= 0;
            if !ok {
                log!("ao: could not create the shaders");
                return None; // Drop releases what exists
            }
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let desc: [u32; 6] = [48, 2, 4, 0x10000, 0, 0];
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &desc, std::ptr::null(), &mut a.cb) < 0 {
                log!("ao: could not create the constant buffer");
                return None;
            }
            // D3D11_BLEND_DESC: AlphaToCoverage, Independent, then 8 x { Enable, Src, Dest, Op, SrcA, DestA, OpA, WriteMask }.
            // RT0: result = dest * src (ZERO, SRC_COLOR, ADD), RGB only: the alpha channel (a material mask the combine reads) is kept.
            let mut bd = [0u32; 2 + 64];
            bd[2..10].copy_from_slice(&[1, 1 /* ZERO */, 3 /* SRC_COLOR */, 1 /* ADD */, 2 /* ONE */, 1 /* ZERO */, 1 /* ADD */, 7 /* RGB */]);
            let create_blend: unsafe extern "system" fn(Obj, *const [u32; 66], *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BLEND_STATE));
            if create_blend(device, &bd, &mut a.multiply) < 0 {
                log!("ao: could not create the blend state");
                return None;
            }
        }
        Some(a)
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            for i in 0..3 {
                release(self.rtv[i]);
                release(self.srv[i]);
                release(self.tex[i]);
                self.rtv[i] = std::ptr::null_mut();
                self.srv[i] = std::ptr::null_mut();
                self.tex[i] = std::ptr::null_mut();
            }
        }
        self.size = (0, 0);
    }

    /// (Re)creates the two visibility targets when the scene size changed.
    unsafe fn ensure_targets(&mut self, dev: Obj, size: (u32, u32)) -> bool {
        if self.size == size && !self.tex[0].is_null() {
            return true;
        }
        unsafe {
            self.free_targets();
            let desc = Tex2dDesc { width: size.0, height: size.1, mips: 1, array: 1, format: R16_FLOAT, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let create_rtv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_RTV));
            let sdesc: [u32; 6] = [R16_FLOAT, 4, 0, 1, 0, 0];
            let rdesc: [u32; 6] = [R16_FLOAT, 4, 0, 0, 0, 0];
            for i in 0..3 {
                if create_tex(dev, &desc, std::ptr::null(), &mut self.tex[i]) < 0
                    || self.tex[i].is_null()
                    || create_srv(dev, self.tex[i], &sdesc, &mut self.srv[i]) < 0
                    || create_rtv(dev, self.tex[i], &rdesc, &mut self.rtv[i]) < 0
                {
                    self.free_targets();
                    return false;
                }
            }
        }
        self.size = size;
        log!("ao: visibility targets {}x{}", size.0, size.1);
        true
    }

    /// SRV of the depth resource, cached while the resource is the same (the SRV keeps it alive, so an
    /// equal pointer is the same resource).
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

    /// Runs the three draws: AO from `depth`, two blurs, multiply into `scene_rtv`. All pipeline state touched
    /// is restored. False when something failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `depth` an R32_FLOAT texture
    /// with a shader-resource bind, of size `p.width` x `p.height`.
    #[allow(dead_code)] // the multiply variant: kept for the tests that pin the shaders
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, scene_rtv: Obj, depth: Obj, p: &AoParams) -> bool {
        unsafe { self.run_inner(dev, ctx, depth, p, Some(scene_rtv)) }
    }

    /// The production path: the same two first draws, then the vertical blur into the visibility target
    /// `vis_srv()` returns (nothing is drawn into the scene; the data path applies it).
    ///
    /// # Safety
    /// As `run`, without the scene target.
    pub unsafe fn run_visibility(&mut self, dev: Obj, ctx: Obj, depth: Obj, p: &AoParams) -> bool {
        unsafe { self.run_inner(dev, ctx, depth, p, None) }
    }

    /// SRV of the finished visibility (valid after `run_visibility`, until the targets are recreated).
    #[allow(dead_code)] // used by the tests
    pub fn vis_srv(&self) -> Obj {
        self.srv[2]
    }

    unsafe fn run_inner(&mut self, dev: Obj, ctx: Obj, depth: Obj, p: &AoParams, scene_rtv: Option<Obj>) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_targets(dev, (p.width, p.height)) || !self.ensure_depth_srv(dev, depth) {
                return false;
            }
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [[f32; 4]; 3]) = cb_values(p);
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            unmap(ctx, self.cb, 0);

            let _state = StateGuard::save(ctx);
            let vp = Viewport { x: 0.0, y: 0.0, w: p.width as f32, h: p.height as f32, min_depth: 0.0, max_depth: 1.0 };
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

            // Null states are the defaults: no culling for a clockwise triangle, scissor off, no depth.
            set_vp(ctx, 1, &vp);
            set_rs(ctx, std::ptr::null_mut());
            set_depth(ctx, std::ptr::null_mut(), 0);
            set_layout(ctx, std::ptr::null_mut());
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, std::ptr::null_mut(), std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);
            let nul: Obj = std::ptr::null_mut();

            // 1: depth -> raw visibility in A.
            set_blend(ctx, std::ptr::null_mut(), factor.as_ptr(), 0xffff_ffff);
            set_rt(ctx, 1, &self.rtv[0], std::ptr::null_mut());
            set_ps(ctx, self.ps_ao, std::ptr::null(), 0);
            set_srv(ctx, 0, 2, [self.depth_srv, nul].as_ptr());
            draw(ctx, 3, 0);
            // 2: horizontal blur A -> B.
            set_rt(ctx, 1, &self.rtv[1], std::ptr::null_mut());
            set_ps(ctx, self.ps_blur, std::ptr::null(), 0);
            set_srv(ctx, 0, 2, [self.depth_srv, self.srv[0]].as_ptr());
            draw(ctx, 3, 0);
            match scene_rtv {
                // 3 (legacy, tests): vertical blur B, then multiply into the scene colour.
                Some(scene_rtv) => {
                    set_rt(ctx, 1, &scene_rtv, std::ptr::null_mut());
                    set_blend(ctx, self.multiply, factor.as_ptr(), 0xffff_ffff);
                    set_ps(ctx, self.ps_apply, std::ptr::null(), 0);
                    set_srv(ctx, 0, 2, [self.depth_srv, self.srv[1]].as_ptr());
                    draw(ctx, 3, 0);
                }
                // 3 (production): vertical blur B into the finished visibility target.
                None => {
                    set_rt(ctx, 1, &self.rtv[2], std::ptr::null_mut());
                    set_ps(ctx, self.ps_blur_v, std::ptr::null(), 0);
                    set_srv(ctx, 0, 2, [self.depth_srv, self.srv[1]].as_ptr());
                    draw(ctx, 3, 0);
                }
            }
        }
        true
    }
}

impl Drop for Ao {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            self.free_targets();
            for o in [self.depth_srv, self.multiply, self.cb, self.ps_blur_v, self.ps_apply, self.ps_blur, self.ps_ao, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from the OMSetRenderTargets hook) ---------------------------------------

static AO: Mutex<Option<Ao>> = Mutex::new(None);
/// The linear depth resource of this frame (a reference we own; 0 = none). Released at the next frame.
static DEPTH: Mutex<(usize,)> = Mutex::new((0,));

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// The 2D texture behind `view` with its description, or None (the caller owns the returned reference).
pub(crate) unsafe fn texture_of(view: Obj) -> Option<(Obj, Tex2dDesc)> {
    unsafe {
        let mut res: Obj = std::ptr::null_mut();
        let get_resource: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(view, vt::VIEW_GET_RESOURCE));
        get_resource(view, &mut res);
        if res.is_null() {
            return None;
        }
        let mut dim = 0u32;
        let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(res, vt::RES_GET_TYPE));
        get_type(res, &mut dim);
        if dim != RESOURCE_TEXTURE2D {
            release(res);
            return None;
        }
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
        get_desc(res, &mut d);
        Some((res, d))
    }
}

/// Is the texture usable as the engine's linear depth: full size, one mip, one sample, 32-bit float and
/// readable as a shader resource.
pub fn depth_ok(d: &Tex2dDesc, screen: (u32, u32)) -> bool {
    (d.width, d.height) == screen
        && d.mips == 1
        && d.array == 1
        && d.samples == 1
        && (d.format == FMT_R32_FLOAT || d.format == R32_TYPELESS)
        && d.bind & 0x8 != 0
}

/// The linear-depth pass is starting: remember its render target's texture for this frame.
///
/// # Safety
/// `rtv` must be a live render-target view.
pub unsafe fn note_depth(rtv: Obj, screen: (u32, u32)) {
    let _ig = IgnoreScope::new();
    unsafe {
        let Some((res, d)) = texture_of(rtv) else { return };
        if !depth_ok(&d, screen) {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            log_once(&LOGGED, || format!("ao: depth source not found: the linear depth target is {}x{} format {} bind {:#x}, not a shader-readable full-size R32_FLOAT", d.width, d.height, d.format, d.bind));
            release(res);
            return;
        }
        let Ok(mut s) = DEPTH.try_lock() else {
            release(res);
            return;
        };
        release(s.0 as Obj);
        s.0 = res as usize;
        CUR.fetch_or(SEEN_DEPTH, Relaxed);
        static FOUND: AtomicBool = AtomicBool::new(false);
        log_once(&FOUND, || format!("ao: depth source found: full-size {}x{} R32_FLOAT linear depth target (first such pass after the opaque pass)", d.width, d.height));
    }
}

/// The visibility texture of this frame for `device`, when the AO ran this frame (the data path binds it to
/// the light-adjust shader). None when the AO is off, did not run, or belongs to another device.
pub fn visibility_srv(device: usize) -> Option<Obj> {
    if !DONE.load(Relaxed) {
        return None;
    }
    let g = AO.try_lock().ok()?;
    let a = g.as_ref()?;
    (a.device == device && !a.srv[2].is_null()).then(|| a.srv[2])
}

/// The effects pass is about to start with `rtv` as RT0: computes the occlusion visibility from the depth
/// (it is applied later, inside the light-adjust shader, see `datapath`). Returns true when it ran. Every failure is logged once and leaves the frame untouched.
///
/// # Safety
/// `ctx` and `rtv` must be live D3D11 objects of the same device.
pub unsafe fn apply(ctx: Obj, rtv: Obj, screen: (u32, u32)) -> bool {
    let _ig = IgnoreScope::new();
    CUR.fetch_or(SEEN_EFFECTS, Relaxed);
    let ok = unsafe { apply_inner(ctx, rtv, screen) };
    if ok {
        DONE.store(true, Relaxed);
        CUR.fetch_or(SEEN_APPLIED, Relaxed);
        static FIRST: AtomicBool = AtomicBool::new(false);
        log_once(&FIRST, || format!("ao: enabled, first frame applied (strength {}, radius {}, {}x{})", strength(), radius(), screen.0, screen.1));
    }
    ok
}

unsafe fn apply_inner(ctx: Obj, rtv: Obj, screen: (u32, u32)) -> bool {
    static NO_DEPTH: AtomicBool = AtomicBool::new(false);
    static BAD_SCENE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    unsafe {
        let blobs = match BLOBS.get() {
            None => return false, // still compiling: skip, try next frame
            Some(Err(_)) => {
                FAILED.store(true, Relaxed);
                return false;
            }
            Some(Ok(b)) => b,
        };
        let Ok(slot) = DEPTH.try_lock() else { return false };
        if slot.0 == 0 {
            log_once(&NO_DEPTH, || "ao: depth source not found for this frame (no usable full-size R32_FLOAT pass before the effects pass); AO skipped".to_string());
            return false;
        }
        let depth = slot.0 as Obj;
        // The effects pass colour target is only inspected (it proves this is the scene's effects pass);
        // nothing is drawn into it any more: the visibility is applied later, inside the light-adjust shader.
        let Some((scene, d)) = texture_of(rtv) else { return false };
        let fmt_ok = d.format == FMT_R16G16B16A16_FLOAT || d.format == FMT_B8G8R8A8_UNORM;
        if (d.width, d.height) != screen || d.mips != 1 || d.array != 1 || d.samples != 1 || !fmt_ok {
            log_once(&BAD_SCENE, || format!("ao: skipped, the effects pass colour target is {}x{} format {} (not a single-sample full-size scene colour)", d.width, d.height, d.format));
            release(scene);
            return false;
        }
        release(scene);
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            return false;
        }
        let ok = 'run: {
            let Ok(mut guard) = AO.try_lock() else { break 'run false };
            if guard.as_ref().is_some_and(|a| a.device != dev as usize) {
                *guard = None;
            }
            if guard.is_none() {
                match Ao::new(dev, blobs) {
                    Some(a) => *guard = Some(a),
                    None => {
                        FAILED.store(true, Relaxed);
                        log!("ao: could not be created; the AO stays off");
                        break 'run false;
                    }
                }
            }
            let a = guard.as_mut().expect("just created");
            let p = AoParams { strength: strength(), radius: radius(), width: screen.0, height: screen.1 };
            let r = a.run_visibility(dev, ctx, depth, &p);
            if !r {
                log_once(&NO_RUN, || "ao: the visibility targets, the depth view or the constants could not be set up; frame left without AO".to_string());
            }
            r
        };
        release(dev);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_consumer_bits_are_distinct_and_clearing_one_keeps_the_others() {
        let mut seen = 0u32;
        for b in ALL_DEPTH_CONSUMERS {
            assert!(b.is_power_of_two(), "{b} is not a single bit");
            assert_eq!(seen & b, 0, "bit {b} used twice");
            seen |= b;
        }
        // Private bits (not asserted by any other test) so parallel tests cannot interfere.
        let (x, y) = (1u32 << 20, 1u32 << 21);
        set_depth_consumer(DEPTH_FOR_STATS, true);
        set_depth_consumer(x, true);
        set_depth_consumer(y, true);
        set_depth_consumer(DEPTH_FOR_STATS, false);
        let c = depth_consumers();
        assert_eq!(c & DEPTH_FOR_STATS, 0);
        assert!(c & x != 0 && c & y != 0, "clearing the stats consumer cleared another one");
        set_depth_consumer(x, false);
        set_depth_consumer(y, false);
    }

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(clamp_strength(9.0), 2.0);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), 1.0);
        assert_eq!(clamp_radius(0.0), 0.25);
        assert_eq!(clamp_radius(100.0), 4.0);
        assert_eq!(clamp_radius(f32::NAN), 1.0);
        assert_eq!(clamp_radius(f32::INFINITY), 4.0);
    }

    #[test]
    fn constants_follow_the_params() {
        let p = AoParams { strength: 1.5, radius: 2.0, width: 3440, height: 1440 };
        let c = cb_values(&p);
        assert_eq!(c[0][0], 1.5);
        assert_eq!(c[0][1], 2.0);
        assert_eq!(c[0][2], TAN_HALF_FOV);
        assert!((c[0][3] - 3440.0 / 1440.0).abs() < 1e-6);
        assert_eq!(c[1][2], 3440.0);
        assert_eq!(c[1][3], 1440.0);
        assert!((c[1][0] - 1.0 / 3440.0).abs() < 1e-9);
        // Out-of-range input is clamped on the way in.
        let q = cb_values(&AoParams { strength: 7.0, radius: 0.0, width: 0, height: 0 });
        assert_eq!((q[0][0], q[0][1]), (2.0, 0.25));
        assert!(q[1].iter().all(|v| v.is_finite()));
    }

    #[test]
    fn depth_target_check() {
        let s = (3440, 1440);
        let ok = Tex2dDesc { width: 3440, height: 1440, mips: 1, array: 1, format: FMT_R32_FLOAT, samples: 1, bind: 0x28, ..Default::default() };
        assert!(depth_ok(&ok, s));
        assert!(depth_ok(&Tex2dDesc { format: R32_TYPELESS, ..ok }, s));
        assert!(!depth_ok(&Tex2dDesc { width: 1720, height: 720, ..ok }, s)); // half-size depth
        assert!(!depth_ok(&Tex2dDesc { bind: 0x20, ..ok }, s)); // not shader-readable
        assert!(!depth_ok(&Tex2dDesc { samples: 4, ..ok }, s)); // multisampled
        assert!(!depth_ok(&Tex2dDesc { format: 87, ..ok }, s)); // wrong format
    }

    #[test]
    fn wants_needs_gate_strength_and_not_done() {
        // Only this test touches ENABLED / DONE / STRENGTH among the host tests.
        let keep = strength();
        set_enabled(false);
        frame_begin();
        set_strength(1.0);
        assert!(!wants());
        set_enabled(true);
        assert!(wants());
        set_strength(0.0);
        assert!(!wants(), "strength 0 disables the pass");
        set_strength(0.5);
        assert!(wants());
        DONE.store(true, Relaxed);
        assert!(!wants());
        frame_begin();
        assert!(wants());
        set_enabled(false);
        set_strength(keep);
    }
}

#[cfg(test)]
#[path = "ao_tests.rs"]
mod gpu_tests;
