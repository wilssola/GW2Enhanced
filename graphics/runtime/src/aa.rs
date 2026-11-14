//! Mod-owned anti-aliasing of the 3D scene: SMAA 1x (HDR-aware, spatial only) and a CAS-style sharpen. See `docs/aa-aniso.md`.
//!
//! WHERE. The engine's own post chain ends in `#16 SMAA` -> `#17 copy (ps#c0ad1686)` -> `#18 UI` -> final pass (see
//! `engine-post-chain.md`). The copy is the last pass that draws the 3D scene alone: the UI is drawn after it onto its
//! target. The data-path rule (`data-path.md`) says an extra full-screen pass inserted between engine passes gets overwritten,
//! so nothing is inserted: at the copy pass's draw (`draw_guard`, called by the `Draw*` hooks while the copy shader is
//! bound) this module runs its own passes in its own targets, reading the copy's input (t0) and writing a mod-owned float16
//! texture, and then binds that texture AS t0 for the engine's own draw. The copy therefore copies the anti-aliased and
//! sharpened picture into the UI target; when the draw is done the guard puts the engine's SRV back (RAII, also on panic).
//! The value flow cannot be bypassed and no engine target is written by the mod.
//!
//! WHAT. 1. luma edges on the tone-compressed colour (HDR highlights make no false edges), 2. blend weights with a 16 pixel
//! search (analytic areas instead of SMAA's tables, no diagonals or corner rounding = SMAA "medium"), 3. neighbourhood
//! blending in the compressed domain (no bright rims around highlights), 4. contrast adaptive sharpening
//! (`sharpen_strength`). Passes 1..3 run only with `smaa=1`, pass 4 only with `sharpen_strength > 0`; with both off nothing
//! runs and nothing is rebound: bit-identical to the engine. Pixels without an edge keep their exact value.
//!
//! The engine's own SMAA (#16) runs before this and is not switched off; this pass finds what it left and gives the HDR-safe
//! result and the sharpen. Shaders are compiled once on a background thread; until then the feature is skipped.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Viewport, ignoring, release, vfn, vt};
use crate::log;
use crate::shafts::{NO_TARGET, Target, make_target};
use crate::{ao, recorder, shaders};

pub const HLSL: &str = include_str!("../shaders/aa.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_EDGES: &str = "ps_edges";
pub const PS_WEIGHTS: &str = "ps_weights";
pub const PS_BLEND: &str = "ps_blend";
pub const PS_CAS: &str = "ps_cas";

/// `ps#c0ad1686`: the engine's final copy before the UI.
pub const COPY_HASH: u32 = 0xc0ad1686;

pub const THRESHOLD_RANGE: (f32, f32) = (0.02, 0.5);
pub const DEFAULT_THRESHOLD: f32 = 0.1;
pub const SHARPEN_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_SHARPEN: f32 = 0.3;
/// SMAA's local contrast adaptation factor.
pub const LOCAL_CONTRAST: f32 = 2.0;

// Slots used (all cleared after the passes): the guard of `StateGuard` restores t0, t1 and b0.
const SLOT_COLOR: u32 = 10;
const SLOT_EDGES: u32 = 11;
const SLOT_WEIGHTS: u32 = 12;
const SLOT_BLEND: u32 = 13;
const SLOT_COUNT: u32 = 4;

const FMT_RGBA16F: u32 = 10;
const FMT_RGBA8: u32 = 28;
const FMT_RG8: u32 = 49;

// ---- settings --------------------------------------------------------------------------------------------

static SMAA: AtomicBool = AtomicBool::new(true);
static THRESHOLD: AtomicU32 = AtomicU32::new(0x3dcc_cccd); // 0.1f32
static SHARPEN: AtomicU32 = AtomicU32::new(0x3e99_999a); // 0.3f32

pub fn clamp_threshold(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_THRESHOLD } else { v.clamp(THRESHOLD_RANGE.0, THRESHOLD_RANGE.1) }
}

pub fn clamp_sharpen(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_SHARPEN } else { v.clamp(SHARPEN_RANGE.0, SHARPEN_RANGE.1) }
}

pub fn set_smaa(on: bool) {
    SMAA.store(on, Relaxed);
}

pub fn smaa() -> bool {
    SMAA.load(Relaxed)
}

pub fn set_threshold(v: f32) -> f32 {
    let v = clamp_threshold(v);
    THRESHOLD.store(v.to_bits(), Relaxed);
    v
}

pub fn threshold() -> f32 {
    f32::from_bits(THRESHOLD.load(Relaxed))
}

pub fn set_sharpen(v: f32) -> f32 {
    let v = clamp_sharpen(v);
    SHARPEN.store(v.to_bits(), Relaxed);
    v
}

pub fn sharpen() -> f32 {
    f32::from_bits(SHARPEN.load(Relaxed))
}

/// Which passes run for these settings: (SMAA passes 1..3, sharpen pass 4). Both false = nothing runs, nothing is rebound.
pub fn passes_for(smaa: bool, sharpen: f32) -> (bool, bool) {
    (smaa, sharpen > 0.0)
}

/// Does the feature run at all: the user did not switch the mod off, the frames are recognised (substitution enabled)
/// and at least one pass is on.
pub fn wanted_for(user_enabled: bool, substitution: bool, smaa: bool, sharpen: f32) -> bool {
    let (a, b) = passes_for(smaa, sharpen);
    user_enabled && substitution && (a || b)
}

fn wanted() -> bool {
    wanted_for(crate::gate::user_enabled(), shaders::substitution_enabled(), smaa(), sharpen())
}

/// Applies the config at start and starts the background compile.
pub fn init(smaa: bool, threshold: f32, sharpen: f32) {
    set_smaa(smaa);
    set_threshold(threshold);
    set_sharpen(sharpen);
    log!("aa: configured (smaa {}, threshold {}, sharpen {})", self::smaa(), self::threshold(), self::sharpen());
    let _ = std::thread::Builder::new().name("gw2g-aa-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("aa: shaders compiled"),
            Err(e) => log!("aa: shader compile failed, the AA stays off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- constants and shaders -------------------------------------------------------------------------------

/// The two constant vectors of `aa.hlsl`.
pub fn cb_values(w: u32, h: u32, threshold: f32, sharpen: f32) -> [[f32; 4]; 2] {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    [[w, h, 1.0 / w, 1.0 / h], [clamp_threshold(threshold), clamp_sharpen(sharpen), LOCAL_CONTRAST, 0.0]]
}

pub struct Blobs {
    pub vs: Vec<u8>,
    pub edges: Vec<u8>,
    pub weights: Vec<u8>,
    pub blend: Vec<u8>,
    pub cas: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, edges: c(PS_EDGES, "ps_5_0")?, weights: c(PS_WEIGHTS, "ps_5_0")?, blend: c(PS_BLEND, "ps_5_0")?, cas: c(PS_CAS, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

// ---- device objects --------------------------------------------------------------------------------------

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;

/// The objects of one device: shaders, constant buffer and the render targets for one picture size.
pub struct Aa {
    device: usize,
    vs: Obj,
    ps_edges: Obj,
    ps_weights: Obj,
    ps_blend: Obj,
    ps_cas: Obj,
    cb: Obj,
    size: (u32, u32),
    edges: Target,
    weights: Target,
    /// The blended picture, only needed when SMAA and sharpen both run.
    tmp: Target,
    /// The finished picture (what the copy pass is given).
    out: Target,
}

// Only touched under the mutex from the render thread.
unsafe impl Send for Aa {}

impl Aa {
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Aa> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut a = Aa { device: device as usize, vs: nul, ps_edges: nul, ps_weights: nul, ps_blend: nul, ps_cas: nul, cb: nul, size: (0, 0), edges: NO_TARGET, weights: NO_TARGET, tmp: NO_TARGET, out: NO_TARGET };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut a.vs) >= 0
                && create_ps(device, b.edges.as_ptr(), b.edges.len(), nul, &mut a.ps_edges) >= 0
                && create_ps(device, b.weights.as_ptr(), b.weights.len(), nul, &mut a.ps_weights) >= 0
                && create_ps(device, b.blend.as_ptr(), b.blend.len(), nul, &mut a.ps_blend) >= 0
                && create_ps(device, b.cas.as_ptr(), b.cas.len(), nul, &mut a.ps_cas) >= 0;
            if !ok {
                log!("aa: could not create the shaders");
                return None; // Drop releases what exists
            }
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const std::ffi::c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &[32, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut a.cb) < 0 || a.cb.is_null() {
                log!("aa: could not create the constant buffer");
                return None;
            }
        }
        Some(a)
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            self.edges.free();
            self.weights.free();
            self.tmp.free();
            self.out.free();
        }
        self.size = (0, 0);
    }

    /// (Re)creates the targets when the picture size changed; `tmp` only when `need_tmp`.
    unsafe fn ensure_targets(&mut self, dev: Obj, size: (u32, u32), need_smaa: bool, need_tmp: bool) -> bool {
        if self.size != size {
            unsafe { self.free_targets() };
        }
        unsafe {
            let mut ok = true;
            if self.out.tex.is_null() {
                match make_target(dev, size.0, size.1, FMT_RGBA16F, false) {
                    Some(t) => self.out = t,
                    None => ok = false,
                }
            }
            if need_smaa && self.edges.tex.is_null() {
                match make_target(dev, size.0, size.1, FMT_RG8, false) {
                    Some(t) => self.edges = t,
                    None => ok = false,
                }
            }
            if need_smaa && self.weights.tex.is_null() {
                match make_target(dev, size.0, size.1, FMT_RGBA8, false) {
                    Some(t) => self.weights = t,
                    None => ok = false,
                }
            }
            if need_tmp && self.tmp.tex.is_null() {
                match make_target(dev, size.0, size.1, FMT_RGBA16F, false) {
                    Some(t) => self.tmp = t,
                    None => ok = false,
                }
            }
            if !ok {
                self.free_targets();
                return false;
            }
        }
        if self.size != size {
            log!("aa: targets {}x{}", size.0, size.1);
        }
        self.size = size;
        true
    }

    /// SRV of the finished picture (valid after `run`, until the targets are recreated).
    #[allow(dead_code)] // used by the tests
    pub fn out_srv(&self) -> Obj {
        self.out.srv
    }

    /// Runs the passes on `color_srv` (a `size` picture) and returns the SRV of the result. All pipeline state touched is
    /// restored. None when something failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `color_srv` a view of a `size` colour texture.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, color_srv: Obj, size: (u32, u32), smaa: bool, threshold: f32, sharpen: f32) -> Option<Obj> {
        let _ig = IgnoreScope::new();
        let (do_smaa, do_cas) = passes_for(smaa, sharpen);
        if !do_smaa && !do_cas {
            return None;
        }
        unsafe {
            if !self.ensure_targets(dev, size, do_smaa, do_smaa && do_cas) {
                return None;
            }
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return None;
            }
            *(mapped[0] as *mut [[f32; 4]; 2]) = cb_values(size.0, size.1, threshold, sharpen);
            unmap(ctx, self.cb, 0);

            let _state = StateGuard::save(ctx);
            let vp = Viewport { x: 0.0, y: 0.0, w: size.0 as f32, h: size.1 as f32, min_depth: 0.0, max_depth: 1.0 };
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
            let clear = || set_srv(ctx, SLOT_COLOR, SLOT_COUNT, [nul; SLOT_COUNT as usize].as_ptr());

            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, factor.as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);
            set_vp(ctx, 1, &vp);

            // What the sharpen reads: the blended picture, or the input when SMAA is off.
            let mut sharpen_src = color_srv;
            if do_smaa {
                // 1: colour -> edges.
                clear();
                set_rt(ctx, 1, &self.edges.rtv, nul);
                set_ps(ctx, self.ps_edges, std::ptr::null(), 0);
                set_srv(ctx, SLOT_COLOR, 1, &color_srv);
                draw(ctx, 3, 0);
                // 2: edges -> blend weights.
                clear();
                set_rt(ctx, 1, &self.weights.rtv, nul);
                set_ps(ctx, self.ps_weights, std::ptr::null(), 0);
                set_srv(ctx, SLOT_EDGES, 1, &self.edges.srv);
                draw(ctx, 3, 0);
                // 3: colour + weights -> blended picture (straight into the output when no sharpen follows).
                clear();
                let dst = if do_cas { &self.tmp } else { &self.out };
                set_rt(ctx, 1, &dst.rtv, nul);
                set_ps(ctx, self.ps_blend, std::ptr::null(), 0);
                set_srv(ctx, SLOT_COLOR, 1, &color_srv);
                set_srv(ctx, SLOT_WEIGHTS, 1, &self.weights.srv);
                draw(ctx, 3, 0);
                sharpen_src = self.tmp.srv;
            }
            if do_cas {
                // 4: sharpen -> output.
                clear();
                set_rt(ctx, 1, &self.out.rtv, nul);
                set_ps(ctx, self.ps_cas, std::ptr::null(), 0);
                set_srv(ctx, SLOT_BLEND, 1, &sharpen_src);
                draw(ctx, 3, 0);
            }
            // Leave nothing of ours bound (the guard restores t0, t1, b0, the targets and the shaders).
            clear();
        }
        Some(self.out.srv)
    }
}

impl Drop for Aa {
    fn drop(&mut self) {
        unsafe {
            self.free_targets();
            for o in [self.cb, self.ps_cas, self.ps_blend, self.ps_weights, self.ps_edges, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from the Draw hooks) ---------------------------------------------------------

static STATE: Mutex<Option<Aa>> = Mutex::new(None);
/// The pixel shader bound on the game's context is the copy before the UI.
static AT_COPY: AtomicBool = AtomicBool::new(false);
/// The picture was already processed this frame.
static DONE: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static APPLIED: AtomicU64 = AtomicU64::new(0);
static COPY_SEEN: AtomicU64 = AtomicU64::new(0);
static SKIPPED_SHAPE: AtomicU64 = AtomicU64::new(0);
static SIZE: AtomicU64 = AtomicU64::new(0);
static LAST_SKIP: AtomicU32 = AtomicU32::new(0);
const SKIP_NONE: u32 = 0;
const SKIP_COMPILING: u32 = 1;
const SKIP_SHAPE: u32 = 2;
const SKIP_RUN: u32 = 3;

/// A new frame starts: the picture may be processed again.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
}

/// `PSSetShader` on `ctx`: remembers whether the engine's pre-UI copy shader is now bound. One atomic load when the feature is off.
pub fn on_ps_set(ctx: Obj, ps: Obj) {
    if !wanted() {
        if AT_COPY.load(Relaxed) {
            AT_COPY.store(false, Relaxed);
        }
        return;
    }
    if ignoring() || recorder::game_context() != ctx as usize {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        let is_copy = !ps.is_null() && shaders::registry().hash_of(ps as usize) == Some(COPY_HASH);
        AT_COPY.store(is_copy, Relaxed);
    });
}

/// Puts the engine's SRV back into t0 when dropped (at the end of the draw hook, after the engine's draw).
pub struct AaGuard {
    restore: Option<(Obj, Obj)>,
}

impl Drop for AaGuard {
    fn drop(&mut self) {
        if let Some((ctx, srv)) = self.restore.take() {
            unsafe {
                let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
                set_srv(ctx, 0, 1, &srv);
                release(srv);
            }
        }
    }
}

/// A draw is about to be issued on `ctx`: when it is the pre-UI copy and the feature is on, runs the passes and binds the
/// result as t0. The returned guard restores t0 after the engine's draw. One atomic load for every other draw; never unwinds.
pub fn draw_guard(ctx: Obj) -> AaGuard {
    if !AT_COPY.load(Relaxed) {
        return AaGuard { restore: None };
    }
    let restore = std::panic::catch_unwind(|| unsafe { apply(ctx) }).ok().flatten().map(|srv| (ctx, srv));
    AaGuard { restore }
}

/// Returns the engine's own t0 SRV (an owned reference) after binding the processed picture, or None (nothing changed).
unsafe fn apply(ctx: Obj) -> Option<Obj> {
    if DONE.load(Relaxed) || ignoring() || recorder::game_context() != ctx as usize || !wanted() || FAILED.load(Relaxed) {
        return None;
    }
    COPY_SEEN.fetch_add(1, Relaxed);
    let blobs = match BLOBS.get() {
        None => {
            LAST_SKIP.store(SKIP_COMPILING, Relaxed);
            return None;
        }
        Some(Err(_)) => {
            FAILED.store(true, Relaxed);
            return None;
        }
        Some(Ok(b)) => b,
    };
    unsafe {
        let get_srvs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        let mut srv: Obj = std::ptr::null_mut();
        get_srvs(ctx, 0, 1, &mut srv);
        if srv.is_null() {
            return None;
        }
        // The shape of the one copy before the UI: a full-size colour target without depth, fed by a texture of the same size.
        let site = recorder::current_site(ctx as usize);
        let shape = match (ao::texture_of(srv), site) {
            (Some((res, d)), Some(site)) => {
                release(res);
                let rt_ok = site.rt0.as_ref().is_some_and(|t| t.w == d.width && t.h == d.height);
                (d.samples == 1 && d.array == 1 && d.mips == 1 && d.width >= 320 && d.height >= 200 && rt_ok && site.depth.is_none()).then_some((d.width, d.height))
            }
            (Some((res, _)), None) => {
                release(res);
                None
            }
            _ => None,
        };
        let Some((width, height)) = shape else {
            release(srv);
            SKIPPED_SHAPE.fetch_add(1, Relaxed);
            LAST_SKIP.store(SKIP_SHAPE, Relaxed);
            return None;
        };
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            release(srv);
            return None;
        }
        let out = 'run: {
            let Ok(mut g) = STATE.try_lock() else { break 'run None };
            if g.as_ref().is_some_and(|a| a.device != dev as usize) {
                *g = None;
            }
            if g.is_none() {
                match Aa::new(dev, blobs) {
                    Some(a) => *g = Some(a),
                    None => {
                        FAILED.store(true, Relaxed);
                        log!("aa: could not be created; the AA stays off");
                        break 'run None;
                    }
                }
            }
            let a = g.as_mut().expect("just created");
            a.run(dev, ctx, srv, (width, height), smaa(), threshold(), sharpen())
        };
        release(dev);
        let Some(out) = out else {
            release(srv);
            LAST_SKIP.store(SKIP_RUN, Relaxed);
            return None;
        };
        let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        set_srv(ctx, 0, 1, &out);
        DONE.store(true, Relaxed);
        LAST_SKIP.store(SKIP_NONE, Relaxed);
        SIZE.store(((width as u64) << 32) | height as u64, Relaxed);
        let n = APPLIED.fetch_add(1, Relaxed) + 1;
        if n == 1 {
            log!("aa: first frame applied (smaa {}, threshold {}, sharpen {}, {}x{})", smaa(), threshold(), sharpen(), width, height);
        }
        Some(srv)
    }
}

/// Dev capture / overlay line.
pub fn status_text() -> String {
    let (a, b) = passes_for(smaa(), sharpen());
    if !a && !b {
        return "off (smaa 0 and sharpen_strength 0)".to_string();
    }
    if !crate::gate::user_enabled() {
        return "off (mod disabled)".to_string();
    }
    let what = format!("{}{}{}", if a { format!("SMAA 1x medium (threshold {:.2})", threshold()) } else { String::new() }, if a && b { " + " } else { "" }, if b { format!("CAS sharpen {:.2}", sharpen()) } else { String::new() });
    if FAILED.load(Relaxed) {
        return format!("{what}: failed (see log), off");
    }
    let n = APPLIED.load(Relaxed);
    let s = SIZE.load(Relaxed);
    if n > 0 {
        return format!("{what}: applied at the pre-UI copy ps#{COPY_HASH:08x}, {n} frames, last {}x{}, skipped shape {}", s >> 32, s & 0xffff_ffff, SKIPPED_SHAPE.load(Relaxed));
    }
    match LAST_SKIP.load(Relaxed) {
        SKIP_COMPILING => format!("{what}: waiting, the shaders are still compiling"),
        SKIP_SHAPE => format!("{what}: skipped, the copy pass does not have the expected shape ({} draws)", SKIPPED_SHAPE.load(Relaxed)),
        SKIP_RUN => format!("{what}: skipped, the targets or constants could not be set up (see log)"),
        _ if BLOBS.get().is_none() => format!("{what}: compiling shaders"),
        _ => format!("{what}: waiting for the pre-UI copy pass (ps#{COPY_HASH:08x} seen {} times)", COPY_SEEN.load(Relaxed)),
    }
}

#[cfg(test)]
#[path = "aa_tests.rs"]
mod tests;
