//! Light halo: a separate, controllable glow around light sources. See `docs/halo.md`.
//!
//! Where: once per frame, right before the light-adjust draw (`datapath::on_draw`, game context). The scene
//! colour is the pixel-shader SRV the game bound at slot 0 for that draw. Our passes write only OUR OWN
//! render targets (the engine overwrites extra draws into its own targets, see `docs/data-path.md`); the
//! result `G` (R16G16B16A16_FLOAT, 1/4 resolution) is bound as `t7` of the light-adjust substitute, which
//! adds it to the linear colour before the exposure remap.
//!
//! Live settings are two atomics copied into a constant buffer on every draw: moving a slider never compiles
//! anything. The shaders are compiled once, on a background thread started by `init`; until that has finished
//! the halo is simply skipped (the render thread never calls D3DCompile).
//!
//! Plain decisions (`clamp_*`, `up_weights`, `level_sizes`, `scene_ok`) are host-testable; `Halo` talks to D3D
//! through raw vtables like `ao.rs` and is exercised on WARP by `halo_tests.rs`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::ao::texture_of;
use crate::d3d::{IgnoreScope, Obj, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::log;
use crate::passes::{FMT_B8G8R8A8_UNORM, FMT_R16G16B16A16_FLOAT};
use crate::shaders;

pub const HLSL: &str = include_str!("../shaders/halo.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_MEAN: &str = "ps_mean";
pub const PS_SOURCE: &str = "ps_source";
pub const PS_DOWN: &str = "ps_down";
pub const PS_UP: &str = "ps_up";
pub const PS_UP_FINAL: &str = "ps_up_final";

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 2.0);
pub const RADIUS_RANGE: (f32, f32) = (0.5, 2.0);
pub const DEFAULT_STRENGTH: f32 = 1.0;
pub const DEFAULT_RADIUS: f32 = 1.0;

/// Weight of each upsample step at radius 1 (level 1 into 0, 2 into 1, 3 into 2). The radius multiplies them.
pub const UP_BASE: [f32; 3] = [0.9, 0.75, 0.6];
/// No upsample step may exceed this (the glow cannot grow without bound with the radius).
pub const UP_MAX: f32 = 1.2;

const R16_FLOAT: u32 = 54;

// ---- switches --------------------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static STRENGTH: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static RADIUS: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
/// What happened this frame / last frame (bit flags below).
static CUR: AtomicU8 = AtomicU8::new(0);
static LAST: AtomicU8 = AtomicU8::new(0);
const SEEN_DRAW: u8 = 1;
const SEEN_APPLIED: u8 = 2;
const SEEN_BAD_SCENE: u8 = 4;

/// Clamps to 0..=2; NaN gives the default (1).
pub fn clamp_strength(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_STRENGTH } else { v.clamp(STRENGTH_RANGE.0, STRENGTH_RANGE.1) }
}

/// Clamps to 0.5..=2; NaN gives the default (1).
pub fn clamp_radius(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_RADIUS } else { v.clamp(RADIUS_RANGE.0, RADIUS_RANGE.1) }
}

pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    v
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Halo, f32::from_bits(STRENGTH.load(Relaxed)))
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

/// The halo should run: on, strength above 0 (0 disables the whole feature), not done this frame.
pub fn wants() -> bool {
    enabled() && strength() > 0.0 && !DONE.load(Relaxed)
}

/// A new frame starts: remembers what the last one did (overlay) and allows the next run.
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
    if !ENABLED.load(Relaxed) {
        return "waiting (classifier gate closed or mod disabled)".to_string();
    }
    let l = LAST.load(Relaxed);
    if l & SEEN_APPLIED != 0 {
        "applied".to_string()
    } else if l & SEEN_BAD_SCENE != 0 {
        "skipped: scene colour not usable (see log)".to_string()
    } else if l & SEEN_DRAW != 0 {
        "skipped: could not run (see log)".to_string()
    } else if BLOBS.get().is_none() {
        "compiling shaders".to_string()
    } else {
        "waiting for a scene frame".to_string()
    }
}

// ---- plain decisions ---------------------------------------------------------------------------

/// The three upsample weights for a radius (level 1 into 0, 2 into 1, 3 into 2).
pub fn up_weights(radius: f32) -> [f32; 3] {
    let r = clamp_radius(radius);
    UP_BASE.map(|b| (b * r).min(UP_MAX))
}

fn div_ceil(a: u32, b: u32) -> u32 {
    a.div_ceil(b).max(1)
}

/// (size of the 1/16 mean target, sizes of the four pyramid levels 1/4, 1/8, 1/16, 1/32) for a scene size.
pub fn level_sizes(w: u32, h: u32) -> ((u32, u32), [(u32, u32); 4]) {
    let (w, h) = (w.max(1), h.max(1));
    let l0 = (div_ceil(w, 4), div_ceil(h, 4));
    let l1 = (div_ceil(l0.0, 2), div_ceil(l0.1, 2));
    let l2 = (div_ceil(l1.0, 2), div_ceil(l1.1, 2));
    let l3 = (div_ceil(l2.0, 2), div_ceil(l2.1, 2));
    ((div_ceil(w, 16), div_ceil(h, 16)), [l0, l1, l2, l3])
}

/// Is `d` usable as the scene colour (the PS SRV at slot 0 of the light-adjust draw), given the size of the
/// pass's render target: same size, single sample/mip/slice, float16 or BGRA, readable as a shader resource.
pub fn scene_ok(d: &Tex2dDesc, target: (u32, u32)) -> bool {
    (d.width, d.height) == target
        && d.width >= 16
        && d.height >= 16
        && d.mips == 1
        && d.array == 1
        && d.samples == 1
        && (d.format == FMT_R16G16B16A16_FLOAT || d.format == FMT_B8G8R8A8_UNORM)
        && d.bind & 0x8 != 0
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HaloParams {
    pub radius: f32,
    pub width: u32,
    pub height: u32,
}

// ---- shaders (compiled once, in the background) -----------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub mean: Vec<u8>,
    pub source: Vec<u8>,
    pub down: Vec<u8>,
    pub up: Vec<u8>,
    pub up_final: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, mean: c(PS_MEAN, "ps_5_0")?, source: c(PS_SOURCE, "ps_5_0")?, down: c(PS_DOWN, "ps_5_0")?, up: c(PS_UP, "ps_5_0")?, up_final: c(PS_UP_FINAL, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Sets the live values from the config and starts the background compile.
pub fn init(strength: f32, radius: f32) {
    set_strength(strength);
    set_radius(radius);
    log!("halo: configured (strength {}, radius {})", self::strength(), self::radius());
    let _ = std::thread::Builder::new().name("gw2g-halo-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("halo: shaders compiled"),
            Err(e) => log!("halo: shader compile failed, the halo stays off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- the halo objects -------------------------------------------------------------------------------

/// One render target with its views.
struct Level {
    tex: Obj,
    srv: Obj,
    rtv: Obj,
    size: (u32, u32),
}

impl Level {
    const fn empty() -> Level {
        Level { tex: std::ptr::null_mut(), srv: std::ptr::null_mut(), rtv: std::ptr::null_mut(), size: (0, 0) }
    }

    unsafe fn free(&mut self) {
        unsafe {
            release(self.rtv);
            release(self.srv);
            release(self.tex);
        }
        *self = Level::empty();
    }

    unsafe fn make(dev: Obj, fmt: u32, size: (u32, u32)) -> Option<Level> {
        let mut l = Level::empty();
        unsafe {
            let desc = Tex2dDesc { width: size.0, height: size.1, mips: 1, array: 1, format: fmt, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let create_rtv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_RTV));
            let sdesc: [u32; 6] = [fmt, 4, 0, 1, 0, 0];
            let rdesc: [u32; 6] = [fmt, 4, 0, 0, 0, 0];
            if create_tex(dev, &desc, std::ptr::null(), &mut l.tex) < 0 || l.tex.is_null() || create_srv(dev, l.tex, &sdesc, &mut l.srv) < 0 || create_rtv(dev, l.tex, &rdesc, &mut l.rtv) < 0 {
                l.free();
                return None;
            }
        }
        l.size = size;
        Some(l)
    }
}

/// Device objects of the halo, valid for one device only.
pub struct Halo {
    device: usize,
    vs: Obj,
    ps_mean: Obj,
    ps_source: Obj,
    ps_down: Obj,
    ps_up: Obj,
    ps_up_final: Obj,
    cb: Obj,
    additive: Obj,
    mean: Level,
    /// 1/4, 1/8, 1/16, 1/32. Level 0 ends up holding the finished halo.
    lv: [Level; 4],
    size: (u32, u32),
}

// The pointers are only used under the halo mutex, from the render thread.
unsafe impl Send for Halo {}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

impl Halo {
    /// Creates the shaders, the constant buffer and the additive blend state on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Halo> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut h = Halo { device: device as usize, vs: nul, ps_mean: nul, ps_source: nul, ps_down: nul, ps_up: nul, ps_up_final: nul, cb: nul, additive: nul, mean: Level::empty(), lv: [Level::empty(), Level::empty(), Level::empty(), Level::empty()], size: (0, 0) };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut h.vs) >= 0
                && create_ps(device, b.mean.as_ptr(), b.mean.len(), nul, &mut h.ps_mean) >= 0
                && create_ps(device, b.source.as_ptr(), b.source.len(), nul, &mut h.ps_source) >= 0
                && create_ps(device, b.down.as_ptr(), b.down.len(), nul, &mut h.ps_down) >= 0
                && create_ps(device, b.up.as_ptr(), b.up.len(), nul, &mut h.ps_up) >= 0
                && create_ps(device, b.up_final.as_ptr(), b.up_final.len(), nul, &mut h.ps_up_final) >= 0;
            if !ok {
                log!("halo: could not create the shaders");
                return None; // Drop releases what exists
            }
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let desc: [u32; 6] = [16, 2, 4, 0x10000, 0, 0];
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &desc, std::ptr::null(), &mut h.cb) < 0 || h.cb.is_null() {
                log!("halo: could not create the constant buffer");
                return None;
            }
            // D3D11_BLEND_DESC: AlphaToCoverage, Independent, then 8 x { Enable, Src, Dest, Op, SrcA, DestA, OpA, WriteMask }.
            // RT0: result = dest + src (ONE, ONE, ADD) on all four channels.
            let mut bd = [0u32; 2 + 64];
            bd[2..10].copy_from_slice(&[1, 2 /* ONE */, 2 /* ONE */, 1 /* ADD */, 2, 2, 1, 15]);
            let create_blend: unsafe extern "system" fn(Obj, *const [u32; 66], *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BLEND_STATE));
            if create_blend(device, &bd, &mut h.additive) < 0 || h.additive.is_null() {
                log!("halo: could not create the blend state");
                return None;
            }
        }
        Some(h)
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            self.mean.free();
            for l in &mut self.lv {
                l.free();
            }
        }
        self.size = (0, 0);
    }

    /// (Re)creates the targets when the scene size changed.
    unsafe fn ensure_targets(&mut self, dev: Obj, size: (u32, u32)) -> bool {
        if self.size == size && !self.lv[0].tex.is_null() {
            return true;
        }
        unsafe {
            self.free_targets();
            let (m, lv) = level_sizes(size.0, size.1);
            let Some(mean) = Level::make(dev, R16_FLOAT, m) else { return false };
            self.mean = mean;
            for (i, s) in lv.iter().enumerate() {
                match Level::make(dev, FMT_R16G16B16A16_FLOAT, *s) {
                    Some(l) => self.lv[i] = l,
                    None => {
                        self.free_targets();
                        return false;
                    }
                }
            }
        }
        self.size = size;
        log!("halo: targets for scene {}x{}: mean {}x{}, levels {:?}", size.0, size.1, self.mean.size.0, self.mean.size.1, self.lv.iter().map(|l| l.size).collect::<Vec<_>>());
        true
    }

    /// SRV of the finished halo `G` (valid after a successful `run`, until the targets are recreated).
    #[allow(dead_code)] // used by the tests
    pub fn glow_view(&self) -> Obj {
        self.lv[0].srv
    }

    /// The texture of the finished halo (tests).
    #[allow(dead_code)]
    pub fn glow_tex(&self) -> Obj {
        self.lv[0].tex
    }

    /// Size of the finished halo texture.
    #[allow(dead_code)]
    pub fn glow_size(&self) -> (u32, u32) {
        self.lv[0].size
    }

    /// Runs the draws with `scene_srv` (gamma ~2.0 colour, `p.width` x `p.height`) as input. All pipeline state
    /// touched is restored. False when something failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `scene_srv` a view of a float16 or
    /// BGRA 2D texture of size `p.width` x `p.height`.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, scene_srv: Obj, p: &HaloParams) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_targets(dev, (p.width, p.height)) {
                return false;
            }
            let _state = StateGuard::save(ctx);
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
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let nul: Obj = std::ptr::null_mut();

            // c0 = (weight, target width, target height, 0), rewritten before every draw that depends on it.
            let write_cb = |weight: f32, size: (u32, u32)| -> bool {
                let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
                if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                    return false;
                }
                *(mapped[0] as *mut [f32; 4]) = [weight, size.0 as f32, size.1 as f32, 0.0];
                unmap(ctx, self.cb, 0);
                true
            };
            let viewport = |size: (u32, u32)| Viewport { x: 0.0, y: 0.0, w: size.0 as f32, h: size.1 as f32, min_depth: 0.0, max_depth: 1.0 };

            // Null states are the defaults: no culling for a clockwise triangle, scissor off, no depth.
            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);
            // The previous frame's halo is still bound as t7 for the game: free it before it becomes a target.
            set_srv(ctx, 7, 1, &nul);
            if !write_cb(0.0, self.size) {
                return false;
            }

            // One pass: target first (so an SRV that was the previous target is released), then the inputs.
            let pass = |rtv: Obj, size: (u32, u32), ps: Obj, a: Obj, b: Obj, additive: bool| {
                let vp = viewport(size);
                set_blend(ctx, if additive { self.additive } else { nul }, factor.as_ptr(), 0xffff_ffff);
                set_rt(ctx, 1, &rtv, nul);
                set_vp(ctx, 1, &vp);
                set_ps(ctx, ps, std::ptr::null(), 0);
                set_srv(ctx, 0, 2, [a, b].as_ptr());
                draw(ctx, 3, 0);
            };

            // 1: scene -> mean of the largest channel per 16x16 block.
            pass(self.mean.rtv, self.mean.size, self.ps_mean, scene_srv, nul, false);
            // 2: scene + mean -> source energy at 1/4.
            pass(self.lv[0].rtv, self.lv[0].size, self.ps_source, scene_srv, self.mean.srv, false);
            // 3: down to 1/8, 1/16, 1/32.
            for i in 1..4 {
                if !write_cb(0.0, self.lv[i].size) {
                    return false;
                }
                pass(self.lv[i].rtv, self.lv[i].size, self.ps_down, self.lv[i - 1].srv, nul, false);
            }
            // 4: up, added into the next larger level. The last step also stores the local mean in alpha.
            let w = up_weights(p.radius);
            for i in (0..3).rev() {
                if !write_cb(w[i], self.lv[i].size) {
                    return false;
                }
                if i == 0 {
                    pass(self.lv[0].rtv, self.lv[0].size, self.ps_up_final, self.lv[1].srv, self.mean.srv, true);
                } else {
                    pass(self.lv[i].rtv, self.lv[i].size, self.ps_up, self.lv[i + 1].srv, nul, true);
                }
            }
            // Do not leave our views bound as inputs of anything (the guard restores slots 0 and 1).
            set_srv(ctx, 0, 2, [nul, nul].as_ptr());
        }
        true
    }
}

impl Drop for Halo {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            self.free_targets();
            for o in [self.additive, self.cb, self.ps_up_final, self.ps_up, self.ps_down, self.ps_source, self.ps_mean, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from the light-adjust draw, see datapath) ------------------------------------

static HALO: Mutex<Option<Halo>> = Mutex::new(None);

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// The halo texture of this frame for `device`, when the halo ran this frame (the data path binds it to the
/// light-adjust shader as `t7`). None when it is off, did not run, or belongs to another device.
pub fn glow_srv(device: usize) -> Option<Obj> {
    if !DONE.load(Relaxed) {
        return None;
    }
    let g = HALO.try_lock().ok()?;
    let h = g.as_ref()?;
    (h.device == device && !h.lv[0].srv.is_null()).then(|| h.lv[0].srv)
}

/// A light-adjust draw is about to be issued on the game's `ctx` (device `dev`): when the halo wants to run
/// this frame, computes it from the scene colour bound at PS slot 0. Returns the halo SRV when it exists this
/// frame. Every failure is logged once and leaves the frame untouched.
///
/// # Safety
/// `ctx` must be the live immediate context of `dev`.
pub unsafe fn prepare(ctx: Obj, dev: Obj) -> Option<Obj> {
    if !wants() {
        return glow_srv(dev as usize);
    }
    let _ig = IgnoreScope::new();
    CUR.fetch_or(SEEN_DRAW, Relaxed);
    let ok = unsafe { prepare_inner(ctx, dev) };
    if ok {
        DONE.store(true, Relaxed);
        CUR.fetch_or(SEEN_APPLIED, Relaxed);
        return glow_srv(dev as usize);
    }
    None
}

unsafe fn prepare_inner(ctx: Obj, dev: Obj) -> bool {
    static BAD_SCENE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    unsafe {
        let blobs = match BLOBS.get() {
            None => return false, // still compiling: skip, try next draw
            Some(Err(_)) => {
                FAILED.store(true, Relaxed);
                return false;
            }
            Some(Ok(b)) => b,
        };
        // The scene colour: the SRV bound at slot 0 of this very draw.
        let mut scene_srv: Obj = std::ptr::null_mut();
        let get_srvs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        get_srvs(ctx, 0, 1, &mut scene_srv);
        if scene_srv.is_null() {
            CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
            log_once(&BAD_SCENE, || "halo: skipped, no shader resource at PS slot 0 of the light-adjust draw".to_string());
            return false;
        }
        // The size of the pass's own target: the scene colour must match it.
        let mut rtv: Obj = std::ptr::null_mut();
        let get_rts: unsafe extern "system" fn(Obj, u32, *mut Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_GET_RTS));
        let mut dsv: Obj = std::ptr::null_mut();
        get_rts(ctx, 1, &mut rtv, &mut dsv);
        release(dsv);
        let target = if rtv.is_null() {
            None
        } else {
            let t = texture_of(rtv);
            release(rtv);
            t.map(|(res, d)| {
                release(res);
                (d.width, d.height)
            })
        };
        let scene = texture_of(scene_srv);
        let (Some(target), Some((res, d))) = (target, scene) else {
            release(scene_srv);
            CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
            log_once(&BAD_SCENE, || "halo: skipped, the scene colour or the render target of the light-adjust draw is not a 2D texture".to_string());
            return false;
        };
        release(res);
        if !scene_ok(&d, target) {
            release(scene_srv);
            CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
            log_once(&BAD_SCENE, || format!("halo: skipped, the scene colour is {}x{} format {} mips {} samples {} (render target {}x{}); need a single-sample float16 or BGRA texture of the target's size", d.width, d.height, d.format, d.mips, d.samples, target.0, target.1));
            return false;
        }
        let ok = 'run: {
            let Ok(mut guard) = HALO.try_lock() else { break 'run false };
            if guard.as_ref().is_some_and(|h| h.device != dev as usize) {
                *guard = None;
            }
            if guard.is_none() {
                match Halo::new(dev, blobs) {
                    Some(h) => *guard = Some(h),
                    None => {
                        FAILED.store(true, Relaxed);
                        log!("halo: could not be created; the halo stays off");
                        break 'run false;
                    }
                }
            }
            let h = guard.as_mut().expect("just created");
            let p = HaloParams { radius: radius(), width: d.width, height: d.height };
            let r = h.run(dev, ctx, scene_srv, &p);
            if !r {
                log_once(&NO_RUN, || "halo: the targets or the constants could not be set up; frame left without halo".to_string());
            }
            r
        };
        release(scene_srv);
        if ok {
            log_once(&FIRST, || format!("halo: first frame applied (scene {}x{}, strength {}, radius {})", d.width, d.height, strength(), radius()));
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(clamp_strength(9.0), 2.0);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), 1.0);
        assert_eq!(clamp_radius(0.0), 0.5);
        assert_eq!(clamp_radius(100.0), 2.0);
        assert_eq!(clamp_radius(f32::NAN), 1.0);
        assert_eq!(clamp_radius(f32::INFINITY), 2.0);
    }

    #[test]
    fn up_weights_follow_the_radius_and_are_capped() {
        assert_eq!(up_weights(1.0), UP_BASE);
        let (lo, hi) = (up_weights(0.5), up_weights(2.0));
        for i in 0..3 {
            assert!(lo[i] < UP_BASE[i] && UP_BASE[i] <= hi[i], "level {i}");
            assert!(hi[i] <= UP_MAX);
        }
        assert_eq!(up_weights(f32::NAN), UP_BASE);
        assert_eq!(up_weights(50.0), up_weights(2.0));
    }

    #[test]
    fn level_sizes_halve_and_never_reach_zero() {
        let (m, lv) = level_sizes(3440, 1440);
        assert_eq!(m, (215, 90));
        assert_eq!(lv, [(860, 360), (430, 180), (215, 90), (108, 45)]);
        let (m, lv) = level_sizes(128, 128);
        assert_eq!(m, (8, 8));
        assert_eq!(lv, [(32, 32), (16, 16), (8, 8), (4, 4)]);
        let (m, lv) = level_sizes(1, 1);
        assert_eq!(m, (1, 1));
        assert!(lv.iter().all(|s| *s == (1, 1)));
        let (_, lv) = level_sizes(0, 0);
        assert!(lv.iter().all(|s| s.0 >= 1 && s.1 >= 1));
    }

    #[test]
    fn scene_check() {
        let ok = Tex2dDesc { width: 3440, height: 1440, mips: 1, array: 1, format: FMT_R16G16B16A16_FLOAT, samples: 1, bind: 0x28, ..Default::default() };
        let t = (3440, 1440);
        assert!(scene_ok(&ok, t));
        assert!(scene_ok(&Tex2dDesc { format: FMT_B8G8R8A8_UNORM, ..ok }, t));
        assert!(!scene_ok(&ok, (1720, 720)), "a target of another size");
        assert!(!scene_ok(&Tex2dDesc { samples: 4, ..ok }, t));
        assert!(!scene_ok(&Tex2dDesc { mips: 5, ..ok }, t));
        assert!(!scene_ok(&Tex2dDesc { array: 2, ..ok }, t));
        assert!(!scene_ok(&Tex2dDesc { bind: 0x20, ..ok }, t), "not shader-readable");
        assert!(!scene_ok(&Tex2dDesc { format: 91, ..ok }, t), "sRGB view would change the values");
        assert!(!scene_ok(&Tex2dDesc { width: 8, height: 8, ..ok }, (8, 8)));
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
        assert!(!wants(), "strength 0 disables the whole feature");
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
#[path = "halo_tests.rs"]
mod gpu_tests;
